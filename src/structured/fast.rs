use std::borrow::Cow;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use libyaml_safer::{
    EventData as YamlEventData, Parser as YamlParser, ScalarStyle as YamlScalarStyle,
};

use super::objects::PredicateObjects;
use super::path::{format_path_from_segments, is_yaml_merge_event, yaml_event_navigate};
use super::predicate::{
    parse_json_predicate_value, parse_toml_predicate_value, parse_yaml_predicate_value,
};
use super::value::{PlainYamlScalarKind, classify_yaml_plain_scalar};
use super::{
    CompiledPredicatePattern, PatternMatcher, StructuredEngine, shift_matches, with_stack_room,
};
use crate::chunks::{adopt_first_chunk, map_chunks, merge_chunks, release_unused_capacity};
use crate::jq_path::{append_array_segment, append_key_segment};
use crate::json_scanner::{
    HitPath, JSON_PARALLEL_SCAN_LEN, JsonStream, ParsedPathQuery, PathSegment,
    json_fold_small_document, json_fold_values_with_paths, json_line_value_start,
    json_navigate_path, json_scalar_text_at, json_scan_values, json_scan_values_prefiltered,
    json_short_stream_value, json_stream_value,
};
use crate::line_end_from;
use crate::profiles::{self, StructuredValueStrategy, structured_value_profile};
use crate::structured_predicate::{PredicateValue, evaluate_clause};
use crate::toml_scanner::{
    toml_fold_values_with_paths, toml_navigate_path, toml_scan_values, toml_scan_values_prefiltered,
};
use crate::yaml_scanner::{
    yaml_fold_values_with_paths, yaml_navigate_path, yaml_scan_values, yaml_scan_values_prefiltered,
};
use crate::{
    SearchConfig, SearchError, StructuredMatch, StructuredMatchKind, StructuredMode,
    StructuredPredicateHandling, short_bytes_eq,
};

struct PathStackEntry {
    path_len: usize,
    expecting_key: bool,
    array_index: Option<usize>,
}

fn match_bytes(bytes: &[u8], case_insensitive: bool) -> Cow<'_, [u8]> {
    if case_insensitive {
        Cow::Owned(crate::fold_ascii_bytes(bytes))
    } else {
        Cow::Borrowed(bytes)
    }
}

fn value_matches(
    matcher: &PatternMatcher,
    bytes: &[u8],
    config: SearchConfig,
    case_insensitive: bool,
) -> bool {
    matcher.any_pattern_matches(match_bytes(bytes, case_insensitive).as_ref(), config)
}

/// Whether a YAML stream has a `---` document marker at a line start. Large inputs are
/// searched in parallel chunks overlapping by one byte less than the marker.
fn has_yaml_document_marker(bytes: &[u8]) -> bool {
    const CHUNK_LEN: usize = profiles::PARALLEL_PASS_CHUNK;

    if bytes.starts_with(b"---") {
        return true;
    }
    if bytes.len() < profiles::PARALLEL_PASS_LEN || !profiles::has_parallel_workers() {
        return has_newline_marker(bytes);
    }
    // Once one chunk finds a marker, the remaining chunks skip their search.
    let found = AtomicBool::new(false);
    map_chunks(bytes.len().div_ceil(CHUNK_LEN), |chunk_index| {
        if found.load(Ordering::Relaxed) {
            return;
        }
        let start = chunk_index * CHUNK_LEN;
        let end = (start + CHUNK_LEN + YAML_MARKER.len() - 1).min(bytes.len());
        if has_newline_marker(&bytes[start..end]) {
            found.store(true, Ordering::Relaxed);
        }
    });
    found.into_inner()
}

/// A `---` document marker with the newline before it.
const YAML_MARKER: &[u8] = b"\n---";

/// Whether `bytes` holds [`YAML_MARKER`]. Each block is first tested branch-free for a newline
/// followed by `-`, which compiles to vector compares and runs at about twice the speed of a
/// substring search, since YAML is full of newlines and dashes but rarely starts a line with
/// a dash; only blocks with such a pair are searched for the whole marker.
fn has_newline_marker(bytes: &[u8]) -> bool {
    const BLOCK: usize = 128;
    let mut start = 0;
    while let Some(window) = bytes.get(start..start + BLOCK + 1) {
        let pairs = window.iter().zip(&window[1..]);
        let dash_lines = pairs.fold(0_u8, |found, (&byte, &next)| {
            found | (u8::from(byte == b'\n') & u8::from(next == b'-'))
        });
        let marker_end = (start + BLOCK + YAML_MARKER.len() - 1).min(bytes.len());
        if dash_lines != 0 && memchr::memmem::find(&bytes[start..marker_end], YAML_MARKER).is_some()
        {
            return true;
        }
        start += BLOCK;
    }
    memchr::memmem::find(&bytes[start..], YAML_MARKER).is_some()
}

/// Whether a YAML stream holds exactly one document: it has no `---` line, or only one that
/// follows nothing but blank lines, comments and directives. Paths in a stream of several
/// documents, including one ending in `---` (an empty second document), start with the
/// document's index. A `...` end marker needs no check, since the parser rejects a document
/// that follows one without a `---`.
fn is_single_yaml_document(bytes: &[u8]) -> bool {
    let preamble: usize = bytes
        .split_inclusive(|&byte| byte == b'\n')
        .take_while(|line| is_yaml_preamble_line(line))
        .map(<[u8]>::len)
        .sum();
    match bytes[preamble..].strip_prefix(b"---") {
        // A marker must end its token; `---x` is the parser's to read.
        Some(body) => {
            matches!(body.first(), None | Some(b' ' | b'\t' | b'\r' | b'\n'))
                && !has_yaml_document_marker(body)
        }
        None => !has_yaml_document_marker(bytes),
    }
}

/// Whether `line` may come before a document's `---`: blank, a comment, or a directive.
fn is_yaml_preamble_line(line: &[u8]) -> bool {
    match line
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        None => true,
        Some(content) => line[content] == b'#' || (content == 0 && line[0] == b'%'),
    }
}

/// `path.ends_with(suffix)`, rejecting early on the last byte.
#[inline(always)]
fn path_ends_with(path: &[u8], suffix: &[u8]) -> bool {
    path.len() >= suffix.len()
        && path.last() == suffix.last()
        && short_bytes_eq(&path[path.len() - suffix.len()..], suffix)
}

type PredicateState<'a> = (&'a CompiledPredicatePattern, PredicateObjects);

/// The values of a JSON stream that one task searches.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamPart {
    /// Where the values start, the first of them at the range's start.
    values: Range<usize>,
    /// If given, the part ends before a value at least this long, as it does before one that
    /// does not parse. Tasks whose first value is only assumed to start where they do end
    /// so, since a wrong assumption then reads no further than that.
    stops_before: Option<usize>,
}

impl StreamPart {
    /// A part that searches every value in `values`.
    fn whole(values: Range<usize>) -> Self {
        Self {
            values,
            stops_before: None,
        }
    }
}

/// Shortest part of a stream of documents worth a task.
const MIN_STREAM_TASK_LEN: usize = 16 * 1024;

/// How many tasks search `len` bytes of a stream of documents: several per thread, so faster
/// threads take more of them, and one without other threads to run them.
fn stream_task_count(len: usize) -> usize {
    const TASKS_PER_THREAD: usize = 4;

    if !profiles::has_parallel_workers() {
        return 1;
    }
    (len / MIN_STREAM_TASK_LEN).clamp(1, TASKS_PER_THREAD * rayon::current_num_threads())
}

/// The length from which a value of a JSON stream of `stream_len` bytes is searched as a
/// document of its own, rather than in a task among the others: twice a thread's even share
/// of the stream, and no less than a document has to be for its search to be split.
fn outsized_value_len(stream_len: usize) -> usize {
    (2 * (stream_len / rayon::current_num_threads())).max(JSON_PARALLEL_SCAN_LEN)
}

/// The bytes of task `task`, which starts at `starts[task]` and ends where the next task
/// starts, or at `end` for the last.
fn task_range(starts: &[usize], task: usize, end: usize) -> Range<usize> {
    starts[task]..starts.get(task + 1).copied().unwrap_or(end)
}

/// Returns task starts for the JSON stream `bytes` from the value at `from` on, assuming that
/// lines start with values: `from`, then the first value of a line after each of
/// `task_count` evenly spread offsets. Each byte is read once, however much whitespace
/// lies between values.
///
/// A line is looked for within `longest_line` bytes of an offset. A longer line holds a
/// value that long, or values that only a search in order tells apart, so no task starts
/// after it at a guess.
fn line_value_starts(
    bytes: &[u8],
    from: usize,
    task_count: usize,
    longest_line: usize,
) -> Vec<usize> {
    let task_len = (bytes.len() - from) / task_count;
    let mut starts = Vec::with_capacity(task_count);
    starts.push(from);
    // Where the search for lines has read to: offsets before it are served already.
    let mut read = from;
    for task in 1..task_count {
        let target = from + task_len * task;
        if target < read {
            continue;
        }
        let searched = &bytes[..bytes.len().min(target + longest_line)];
        read = match json_line_value_start(searched, target) {
            Some(line) => {
                starts.push(line);
                line
            }
            None => searched.len(),
        };
    }
    starts
}

/// What one task searches of a JSON stream whose values are read in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StreamItem {
    /// The values that start in a range, each shorter than an outsized one.
    Values(Range<usize>),
    /// The span of one outsized value.
    Outsized((usize, usize)),
}

/// Reads the values of the JSON stream `bytes` in order from the value at `from`, each once,
/// and returns them in stream order as items to search: values shorter than `outsized_len`
/// in runs of a task's length, and every value at least that long alone.
///
/// Reading ends at a value that does not parse, which the last item holds along with the
/// rest of the stream.
fn read_stream_items(bytes: &[u8], from: usize, outsized_len: usize) -> Vec<StreamItem> {
    let task_len = (bytes.len() - from) / stream_task_count(bytes.len() - from);
    let mut items = Vec::new();
    // Where the values that no item holds yet start.
    let mut run_start = None;
    let mut next = from;
    while next < bytes.len()
        && let Some(((start, end), following)) = json_stream_value(bytes, next, bytes.len())
    {
        if end - start >= outsized_len {
            if let Some(run) = run_start.take() {
                items.push(StreamItem::Values(run..start));
            }
            items.push(StreamItem::Outsized((start, end)));
        } else if following - *run_start.get_or_insert(start) >= task_len {
            items.extend(
                run_start
                    .take()
                    .map(|run| StreamItem::Values(run..following)),
            );
        }
        next = following;
    }
    let rest = run_start.unwrap_or(next);
    if rest < bytes.len() {
        items.push(StreamItem::Values(rest..bytes.len()));
    }
    items
}

/// Runs `search` for tasks `0..task_count` on the pool, appending the matches they return to
/// `matches` in task order.
///
/// # Errors
///
/// Returns the error of the first task in order that fails. Tasks not yet started by then
/// are skipped.
fn append_task_matches<'a>(
    task_count: usize,
    search: impl Fn(usize) -> Result<Vec<StructuredMatch<'a>>, SearchError> + Sync,
    matches: &mut Vec<StructuredMatch<'a>>,
) -> Result<(), SearchError> {
    let stop = AtomicBool::new(false);
    let mut failed = None;
    merge_chunks(
        task_count,
        |task| (!stop.load(Ordering::Relaxed)).then(|| search(task)),
        |_, searched| match searched {
            Some(Ok(mut found)) => {
                if failed.is_none() {
                    matches.append(&mut found);
                }
            }
            Some(Err(error)) => {
                stop.store(true, Ordering::Relaxed);
                failed.get_or_insert(error);
            }
            None => {}
        },
    );
    failed.map_or(Ok(()), Err)
}

/// Merges per-chunk predicate states in document order; `None` when there are no chunks.
fn merge_predicate_chunks(chunks: Vec<Vec<PredicateState<'_>>>) -> Option<Vec<PredicateState<'_>>> {
    let mut chunks = chunks.into_iter();
    let mut states = chunks.next()?;
    let later_chunks = chunks.collect::<Vec<_>>();
    for (pattern_index, (_, objects)) in states.iter_mut().enumerate() {
        objects.reserve_for(later_chunks.iter().map(|later| &later[pattern_index].1));
    }
    for later_states in later_chunks {
        for ((_, objects), (_, later_objects)) in states.iter_mut().zip(later_states) {
            objects.absorb(later_objects);
        }
    }
    Some(states)
}

/// Builds a match; `node_text` must be the haystack text at `span`.
pub(super) fn make_structured_match<'a>(
    node_text: &'a str,
    (start, end): (usize, usize),
    pattern_index: usize,
    path: String,
    match_kind: StructuredMatchKind,
) -> StructuredMatch<'a> {
    StructuredMatch {
        start,
        end,
        pattern_index,
        path,
        node_text,
        match_kind,
    }
}

impl StructuredEngine {
    pub(super) fn can_use_fast_predicates(&self, config: SearchConfig) -> bool {
        !self.compiled_predicates.is_empty()
            && self.path_matcher.is_none()
            && self.value_matcher.is_none()
            && self.path_value_matcher.is_none()
            && config.structured_predicate_handling() == StructuredPredicateHandling::Ignore
    }

    /// Searches a single-kind pattern set with the format's byte scanner (or libyaml events).
    /// Returns `None` when the full parser must handle the search, so it reports parse errors.
    pub(super) fn fast_search<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        config: SearchConfig,
        mode: StructuredMode,
        limit: usize,
    ) -> Option<Result<Vec<StructuredMatch<'a>>, SearchError>> {
        let bytes = haystack.as_bytes();
        let kind = self.fast_query_kind()?;
        // Paths in a stream of several documents start with the document's `$[i]`, which
        // only the parser assigns. A path starting with a key therefore never matches there,
        // so its search needs the check only once it finds something; one starting with an
        // index needs it first. Value queries check for markers themselves.
        let yaml_paths = mode == StructuredMode::Yaml
            && matches!(
                kind,
                StructuredMatchKind::Path | StructuredMatchKind::PathValue
            );
        let indexes_root = yaml_paths
            && self
                .parsed_path_queries
                .iter()
                .chain(&self.parsed_path_value_queries)
                .any(|query| matches!(query.segments.first(), Some(PathSegment::Index(_))));
        match mode {
            StructuredMode::Json => {
                let first = bytes
                    .iter()
                    .find(|b| !matches!(**b, b' ' | b'\t' | b'\n' | b'\r'));
                let last = bytes
                    .iter()
                    .rev()
                    .find(|b| !matches!(**b, b' ' | b'\t' | b'\n' | b'\r'));
                match (first, last) {
                    (Some(b'{'), Some(b'}')) | (Some(b'['), Some(b']')) => {}
                    _ => return None,
                }
            }
            StructuredMode::Yaml if indexes_root && !is_single_yaml_document(bytes) => {
                return None;
            }
            StructuredMode::Yaml => {}
            StructuredMode::Toml => {
                // Spot-check the first header; malformed headers need the parser.
                if let Some(first_bracket) = memchr::memchr(b'[', bytes)
                    && (first_bracket == 0 || bytes[first_bracket - 1] == b'\n')
                {
                    let line_end = line_end_from(bytes, first_bracket);
                    if !bytes[first_bracket..line_end].contains(&b']') {
                        return None;
                    }
                }
            }
        }
        match kind {
            StructuredMatchKind::Path | StructuredMatchKind::PathValue => {
                let found = self.fast_path_search::<P>(haystack, mode, limit, kind)?;
                let unchecked_match = yaml_paths
                    && !indexes_root
                    && found.as_ref().is_ok_and(|matches| !matches.is_empty());
                if unchecked_match && !is_single_yaml_document(bytes) {
                    return None;
                }
                Some(found)
            }
            StructuredMatchKind::Value if mode == StructuredMode::Yaml => {
                self.fast_yaml_value_search::<P>(haystack, config, limit)
            }
            StructuredMatchKind::Value => self
                .fast_value_search::<P>(haystack, config, mode, limit)
                .map(Ok),
            StructuredMatchKind::Predicate => None,
        }
    }

    pub(super) fn fast_predicate_search<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        mode: StructuredMode,
    ) -> Option<Vec<StructuredMatch<'a>>> {
        let bytes = haystack.as_bytes();
        // Expands one monomorphized fold per format, keeping dispatch out of the callback.
        macro_rules! fold {
            ($lazy:literal, $scanner:path, [$($prefilter:expr)?],
             |$start:ident, $end:ident| $parse:expr) => {{
                $scanner(
                    bytes,
                    $($prefilter,)?
                    || self.new_predicate_states(),
                    |states, path, $start, $end| {
                        self.record_predicate_value::<$lazy>(states, path, ($start, $end), || {
                            $parse
                        });
                    },
                )
            }};
        }
        let chunks = match mode {
            StructuredMode::Json if self.parses_json_predicate_values_lazily() => {
                fold!(true, json_fold_values_with_paths, [], |start, end| {
                    parse_json_predicate_value(haystack, start, end)
                })
            }
            StructuredMode::Json => {
                fold!(false, json_fold_values_with_paths, [], |start, end| {
                    parse_json_predicate_value(haystack, start, end)
                })
            }
            StructuredMode::Yaml => fold!(
                false,
                yaml_fold_values_with_paths,
                [self.predicate_item_needle.as_ref()],
                |start, end| parse_yaml_predicate_value(&haystack[start..end])
            )?,
            StructuredMode::Toml => fold!(false, toml_fold_values_with_paths, [], |start, end| {
                parse_toml_predicate_value(&haystack[start..end])
            })?,
        };
        let mut matches = Vec::new();
        let states = merge_predicate_chunks(chunks)?;
        self.finish_predicate_states::<P>(haystack, &states, &mut matches);
        Some(matches)
    }

    /// Whether JSON values are parsed only once a clause's path matches theirs, which pays
    /// off for one pattern of several clauses.
    fn parses_json_predicate_values_lazily(&self) -> bool {
        matches!(self.compiled_predicates.as_slice(), [pattern] if pattern.clauses.len() > 1)
    }

    /// Searches the stream of JSON values that `extent` finds in `haystack`, each value as
    /// its own document, as jq reads JSON Lines: paths start at `$` in every value.
    ///
    /// Unlimited searches of long streams first run in tasks that each start at the first
    /// value of a line, as the values of JSON Lines do. A task's matches are kept if the task
    /// before it ended at that value. From the first task that didn't, or that met a value
    /// of [`outsized_value_len`], [`Self::search_stream_in_order`] searches the rest, as it
    /// does the whole stream if the first value is that long or lines start no second task.
    ///
    /// # Returns
    ///
    /// Up to `limit` matches in stream order.
    ///
    /// # Errors
    ///
    /// Returns the error of a value that [`Self::search_one_document`] fails to search.
    pub(super) fn search_json_stream<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        extent: JsonStream,
        config: SearchConfig,
        limit: usize,
    ) -> Result<Vec<StructuredMatch<'a>>, SearchError> {
        let stream = &haystack[..extent.end];
        let bytes = stream.as_bytes();
        let (first_start, first_end) = extent.first;
        let task_count = stream_task_count(bytes.len() - first_start);
        if limit < usize::MAX || task_count < 2 {
            let part = StreamPart::whole(first_start..bytes.len());
            return with_stack_room(|| self.search_stream_part::<P>(stream, part, config, limit))
                .map(|(matches, _)| matches);
        }

        let outsized_len = outsized_value_len(bytes.len() - first_start);
        let mut matches = Vec::new();
        let starts = (first_end - first_start < outsized_len)
            .then(|| line_value_starts(bytes, first_start, task_count, outsized_len))
            .filter(|starts| starts.len() > 1);
        let Some(starts) = starts else {
            self.search_stream_in_order::<P>(stream, first_start, config, &mut matches)?;
            return Ok(matches);
        };
        // Set once a task did not end where the next one started, or failed.
        let stop = AtomicBool::new(false);
        // Where the tasks whose matches were kept ended, if not at the stream's end.
        let mut resume = None;
        let mut failed = None;
        merge_chunks(
            starts.len(),
            |task| {
                let part = StreamPart {
                    values: task_range(&starts, task, bytes.len()),
                    stops_before: Some(outsized_len),
                };
                // Stack room for all of a part's values: the byte scanners recurse per level.
                (!stop.load(Ordering::Relaxed)).then(|| {
                    with_stack_room(|| {
                        self.search_stream_part::<P>(stream, part, config, usize::MAX)
                    })
                })
            },
            |task, searched| {
                if resume.is_some() || failed.is_some() {
                    return;
                }
                match searched {
                    Some(Ok((mut part_matches, next))) => {
                        if task == 0 {
                            matches = adopt_first_chunk(part_matches, starts.len(), None);
                        } else {
                            matches.append(&mut part_matches);
                        }
                        if next != task_range(&starts, task, bytes.len()).end {
                            stop.store(true, Ordering::Relaxed);
                            resume = Some(next);
                        }
                    }
                    Some(Err(error)) => {
                        stop.store(true, Ordering::Relaxed);
                        failed = Some(error);
                    }
                    // Only tasks after the one that set `stop` are skipped.
                    None => {}
                }
            },
        );
        if let Some(error) = failed {
            return Err(error);
        }
        if let Some(resume) = resume {
            self.search_stream_in_order::<P>(stream, resume, config, &mut matches)?;
        }
        release_unused_capacity(&mut matches);
        Ok(matches)
    }

    /// Searches the values of a JSON stream from the one at `from` on, appending every match
    /// to `matches` in stream order.
    ///
    /// The calling thread reads the values in order, which shares them out at true value
    /// starts ([`read_stream_items`]), and the pool then searches the items. A value of
    /// [`outsized_value_len`] is an item of its own, searched as a document: in a task
    /// among other values it would keep one thread busy while the others run out of work,
    /// whereas the search of a document is shared with idle threads where it can be split.
    ///
    /// # Errors
    ///
    /// Returns the error of the first value in stream order that
    /// [`Self::search_one_document`] fails to search.
    fn search_stream_in_order<'a, P: HitPath>(
        &self,
        stream: &'a str,
        from: usize,
        config: SearchConfig,
        matches: &mut Vec<StructuredMatch<'a>>,
    ) -> Result<(), SearchError> {
        let bytes = stream.as_bytes();
        let items = read_stream_items(bytes, from, outsized_value_len(bytes.len() - from));
        let search_item = |item: usize| match &items[item] {
            StreamItem::Values(values) => {
                let part = StreamPart::whole(values.clone());
                with_stack_room(|| self.search_stream_part::<P>(stream, part, config, usize::MAX))
                    .map(|(found, _)| found)
            }
            StreamItem::Outsized(value) => self.search_stream_value::<P>(stream, *value, config),
        };
        let outsized_among_others = items.len() > 1
            && items
                .iter()
                .any(|item| matches!(item, StreamItem::Outsized(_)));
        if outsized_among_others {
            // The scope only moves the search of the items onto a thread of the pool,
            // where the search of an outsized value can share its work with idle threads.
            // A thread outside the pool that is busy with the items cannot.
            rayon::scope(|_| append_task_matches(items.len(), search_item, matches))
        } else {
            append_task_matches(items.len(), search_item, matches)
        }
    }

    /// Searches the value that spans `start..end` of a JSON stream as a document of its
    /// own, which splits the search of a long one across threads where it can be split.
    ///
    /// # Errors
    ///
    /// Returns the error of [`Self::search_one_document`].
    fn search_stream_value<'a, P: HitPath>(
        &self,
        stream: &'a str,
        (start, end): (usize, usize),
        config: SearchConfig,
    ) -> Result<Vec<StructuredMatch<'a>>, SearchError> {
        let document = &stream[start..end];
        let mut matches =
            with_stack_room(|| self.search_one_document::<P>(document, config, usize::MAX))?;
        shift_matches(&mut matches, start);
        Ok(matches)
    }

    /// Searches the values of a JSON stream that start in `part`, each as its own document,
    /// until `limit` matches are found or the part ends early.
    ///
    /// Predicate patterns reuse their states and the path buffer from value to value, so a
    /// value without a match allocates nothing.
    ///
    /// # Returns
    ///
    /// The matches in stream order, and where the first value not searched starts.
    ///
    /// # Errors
    ///
    /// Returns the error of a value that [`Self::search_one_document`] fails to search.
    fn search_stream_part<'a, P: HitPath>(
        &self,
        stream: &'a str,
        part: StreamPart,
        config: SearchConfig,
        limit: usize,
    ) -> Result<(Vec<StructuredMatch<'a>>, usize), SearchError> {
        /// Room for the paths of deeply nested values without growing.
        const PATH_CAPACITY: usize = 128;

        let bytes = stream.as_bytes();
        let folds = self.can_use_fast_predicates(config);
        let lazy = self.parses_json_predicate_values_lazily();
        let mut states = self.new_predicate_states();
        let mut path = Vec::with_capacity(if folds { PATH_CAPACITY } else { 0 });
        let mut matches = Vec::new();
        let mut next = part.values.start;
        while next < part.values.end && matches.len() < limit {
            let value = match part.stops_before {
                Some(len) => json_short_stream_value(bytes, next, bytes.len(), len),
                // A value that does not parse spans the rest of the stream.
                None => json_stream_value(bytes, next, bytes.len())
                    .or(Some(((next, bytes.len()), bytes.len()))),
            };
            let Some(((start, end), following)) = value else {
                break;
            };
            next = following;
            let document = &stream[start..end];
            let matches_before = matches.len();
            let folded = folds
                && if lazy {
                    self.fold_stream_value::<true>(document, &mut path, &mut states)
                } else {
                    self.fold_stream_value::<false>(document, &mut path, &mut states)
                };
            if !folded {
                let room = limit - matches_before;
                matches.append(&mut self.search_one_document::<P>(document, config, room)?);
            } else if states.iter().any(|(_, objects)| !objects.is_empty()) {
                self.finish_predicate_states::<P>(document, &states, &mut matches);
                states.iter_mut().for_each(|(_, objects)| objects.clear());
            }
            shift_matches(&mut matches[matches_before..], start);
            matches.truncate(limit);
        }
        Ok((matches, next))
    }

    /// Records in `states` the clauses that the scalars of `document`, one value of a JSON
    /// stream, satisfy, building paths in `path`. See [`Self::evaluate_predicate_hit`] for
    /// `LAZY`.
    ///
    /// # Returns
    ///
    /// Whether the value was read: one long enough to be split across threads is left to
    /// [`Self::search_one_document`].
    fn fold_stream_value<const LAZY: bool>(
        &self,
        document: &str,
        path: &mut Vec<u8>,
        states: &mut Vec<PredicateState<'_>>,
    ) -> bool {
        let visit = |states: &mut Vec<PredicateState<'_>>, path: &[u8], start, end| {
            self.record_predicate_value::<LAZY>(states, path, (start, end), || {
                parse_json_predicate_value(document, start, end)
            });
        };
        json_fold_small_document(document.as_bytes(), path, states, &visit)
    }

    /// Evaluates the scalar at `span`, found at `path`, against the clauses whose paths end
    /// as `path` does, recording the ones it satisfies in `states`. See
    /// [`Self::evaluate_predicate_hit`] for `LAZY`.
    #[inline(always)]
    fn record_predicate_value<'value, const LAZY: bool>(
        &self,
        states: &mut [PredicateState<'_>],
        path: &[u8],
        span: (usize, usize),
        parse: impl FnMut() -> Option<PredicateValue<'value>>,
    ) {
        if path
            .last()
            .is_some_and(|&last| self.predicate_suffix_last_bytes[usize::from(last)])
            && (LAZY || self.predicate_path_matches(path))
        {
            Self::evaluate_predicate_hit::<LAZY>(path, span, states, parse);
        }
    }

    fn predicate_path_matches(&self, path: &[u8]) -> bool {
        self.compiled_predicates.iter().any(|pattern| {
            pattern
                .clauses
                .iter()
                .any(|clause| path_ends_with(path, &clause.suffix_bytes))
        })
    }

    fn new_predicate_states(&self) -> Vec<PredicateState<'_>> {
        self.compiled_predicates
            .iter()
            .map(|pattern| (pattern, PredicateObjects::new(pattern.clauses.len())))
            .collect()
    }

    // With `LAZY`, the value is parsed only once some clause suffix matches (used for JSON
    // conjunctions); otherwise it is parsed up front.
    #[inline]
    fn evaluate_predicate_hit<'value, const LAZY: bool>(
        path: &[u8],
        span: (usize, usize),
        states: &mut [PredicateState<'_>],
        mut parse: impl FnMut() -> Option<PredicateValue<'value>>,
    ) {
        let mut parsed_value = None;
        if !LAZY {
            parsed_value = parse();
            if parsed_value.is_none() {
                return;
            }
        }
        for (pattern, objects) in states {
            for clause in &pattern.clauses {
                if !path_ends_with(path, &clause.suffix_bytes) {
                    continue;
                }
                if LAZY && parsed_value.is_none() {
                    parsed_value = parse();
                }
                let Some(value) = parsed_value else {
                    return;
                };
                if matches!(evaluate_clause(value, &clause.operator), Ok(true)) {
                    let prefix_len = path.len() - clause.suffix_bytes.len();
                    objects.record(&path[..prefix_len], clause.clause_index, span);
                }
            }
        }
    }

    /// Appends the matches of `states`, one per object that satisfies every clause of its
    /// pattern, to `matches` in document order.
    fn finish_predicate_states<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        states: &[PredicateState<'_>],
        matches: &mut Vec<StructuredMatch<'a>>,
    ) {
        let found = matches.len();
        for (pattern, objects) in states {
            let first_suffix = pattern.clauses[0].suffix_bytes.as_slice();
            let mut pattern_matches = objects.map_complete(|object_path, span| {
                let path = P::build(|| {
                    let mut path = Vec::with_capacity(object_path.len() + first_suffix.len());
                    path.extend_from_slice(object_path);
                    path.extend_from_slice(first_suffix);
                    String::from_utf8(path).expect("path is valid UTF-8")
                });
                make_structured_match(
                    &haystack[span.0..span.1],
                    span,
                    pattern.pattern_index,
                    path.into_string(),
                    StructuredMatchKind::Predicate,
                )
            });
            pattern_matches.sort_unstable_by_key(|matched| matched.start);
            matches.append(&mut pattern_matches);
        }
        // Merges the patterns' runs into document order; ties keep pattern order.
        if states.len() > 1 {
            matches[found..].sort_by_key(|matched| matched.start);
        }
    }

    #[inline(always)]
    fn fast_path_search<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        mode: StructuredMode,
        limit: usize,
        match_kind: StructuredMatchKind,
    ) -> Option<Result<Vec<StructuredMatch<'a>>, SearchError>> {
        let path_value = match_kind == StructuredMatchKind::PathValue;
        let (queries, toml_paths) = if path_value {
            (&self.parsed_path_value_queries, &self.toml_path_value_paths)
        } else {
            (&self.parsed_path_queries, &self.toml_paths)
        };
        let bytes = haystack.as_bytes();
        // With a limit below the query count, per-query navigation can stop early, unlike
        // the batch.
        let spans = self
            .json_path_batch
            .as_ref()
            .filter(|_| mode == StructuredMode::Json && !path_value && limit >= queries.len())
            .map(|batch| batch.navigate(bytes));
        let mut matches = Vec::with_capacity(limit.min(queries.len()));
        for (query_index, query) in queries.iter().enumerate() {
            let expected = if path_value {
                match query.expected_value.as_deref() {
                    Some(expected) => Some(expected),
                    None if mode == StructuredMode::Yaml => continue,
                    None => return None,
                }
            } else {
                None
            };
            let span = match mode {
                StructuredMode::Json => match &spans {
                    Some(spans) => spans[query_index],
                    None => json_navigate_path(bytes, &query.segments),
                },
                StructuredMode::Yaml => match yaml_navigate_path(bytes, &query.segments) {
                    Some(span) => Some(span),
                    None => match yaml_event_navigate(haystack, &query.segments)? {
                        Ok(span) => span,
                        Err(error) => return Some(Err(error)),
                    },
                },
                // TOML navigation misses are inconclusive, so `?` defers to the parser.
                StructuredMode::Toml => {
                    let path = toml_paths.get(query_index)?.as_ref()?;
                    Some(toml_navigate_path(bytes, path)?)
                }
            };
            let Some(span @ (start, end)) = span else {
                continue;
            };
            // A quote right after a closing quote means the scalar continues (a YAML doubled
            // quote or a TOML multi-line string); leave it to the parser.
            let goes_on = mode != StructuredMode::Json
                && start < end
                && matches!(bytes[start], b'"' | b'\'')
                && bytes.get(end) == Some(&bytes[start]);
            if goes_on {
                return None;
            }
            if let Some(expected) = expected {
                let raw = &haystack[start..end];
                let scalar = match mode {
                    StructuredMode::Json => json_scalar_text_at(bytes, start, end),
                    // `?` defers to the parser when the written text is ambiguous.
                    StructuredMode::Yaml => Some(Cow::Borrowed(yaml_written_scalar_text(raw)?)),
                    StructuredMode::Toml => Some(Cow::Borrowed(toml_written_scalar_text(raw)?)),
                };
                if !scalar.is_some_and(|scalar| scalar.as_ref() == expected) {
                    continue;
                }
            }
            matches.push(make_structured_match(
                &haystack[start..end],
                span,
                query.pattern_index,
                P::build(|| format_path_from_segments(&query.segments)).into_string(),
                match_kind,
            ));
            if matches.len() == limit {
                break;
            }
        }
        Some(Ok(matches))
    }

    fn fast_value_search<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        config: SearchConfig,
        mode: StructuredMode,
        limit: usize,
    ) -> Option<Vec<StructuredMatch<'a>>> {
        let matcher = self.value_matcher.as_ref()?;
        let bytes = haystack.as_bytes();
        let case_insensitive = config.case_mode() == crate::CaseMode::AsciiInsensitive;
        let needle = matcher.prefilter_needle();
        let pattern_len = needle.map_or(0, |needle| needle.as_bytes().len());
        let profile = structured_value_profile(bytes.len(), pattern_len, case_insensitive);
        // The prefilter needle folds case itself, so its matcher reads values unfolded.
        let needle = needle.filter(|_| profile.strategy == StructuredValueStrategy::Prefiltered);
        let matches_value = |value: &[u8]| matcher.any_pattern_matches(value, config);
        let matches_naive = |value: &[u8]| value_matches(matcher, value, config, case_insensitive);
        let collect = |start, end, path: P| {
            let node_text = &haystack[start..end];
            let path = path.into_string();
            make_structured_match(node_text, (start, end), 0, path, StructuredMatchKind::Value)
        };
        // A macro, since the scanners differ in hit types and only JSON is infallible.
        macro_rules! scan {
            ($plain:path, $prefiltered:path, $optional:path $(, $depth:expr)?) => {{
                let hits = $optional(match needle {
                    Some(needle) => $prefiltered(
                        bytes, needle, $($depth,)? limit, matches_value,
                    ),
                    None => $plain(bytes, limit, matches_naive),
                })?;
                Some(hits.into_iter().map(|hit| collect(hit.start, hit.end, hit.path)).collect())
            }};
        }
        match mode {
            StructuredMode::Json => scan!(
                json_scan_values,
                json_scan_values_prefiltered,
                Some,
                profile.prefilter_min_depth
            ),
            StructuredMode::Yaml => scan!(
                yaml_scan_values,
                yaml_scan_values_prefiltered,
                std::convert::identity
            ),
            StructuredMode::Toml => scan!(
                toml_scan_values,
                toml_scan_values_prefiltered,
                std::convert::identity
            ),
        }
    }

    /// The single pattern kind the scanners can answer for this pattern set, if any. Mixed
    /// kinds, unparsed paths, and multiple value patterns need the full parser.
    fn fast_query_kind(&self) -> Option<StructuredMatchKind> {
        let all_parsed = |matcher: &PatternMatcher, queries: &[ParsedPathQuery]| {
            matcher.pattern_indexes.len() == queries.len()
        };
        match (
            &self.path_matcher,
            &self.value_matcher,
            &self.path_value_matcher,
        ) {
            (Some(paths), None, None) if all_parsed(paths, &self.parsed_path_queries) => {
                Some(StructuredMatchKind::Path)
            }
            (None, None, Some(path_values))
                if all_parsed(path_values, &self.parsed_path_value_queries) =>
            {
                Some(StructuredMatchKind::PathValue)
            }
            (None, Some(values), None) if values.pattern_indexes.len() == 1 => {
                Some(StructuredMatchKind::Value)
            }
            (Some(_), None, None)
            | (None, None, Some(_))
            | (None, Some(_), None)
            | (None, None, None)
            | (Some(_), Some(_), _)
            | (Some(_), None, Some(_))
            | (None, Some(_), Some(_)) => None,
        }
    }

    /// Finds value matches in a single-document YAML stream; `None` for multi-document ones.
    fn fast_yaml_value_search<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        config: SearchConfig,
        limit: usize,
    ) -> Option<Result<Vec<StructuredMatch<'a>>, SearchError>> {
        // An unlimited scan reaches any marker itself and gives up, so checking first would
        // read the document twice. A limited scan may stop before a marker, so check first.
        let scan_meets_markers = limit == usize::MAX;
        if !scan_meets_markers && has_yaml_document_marker(haystack.as_bytes()) {
            return None;
        }
        if let Some(matches) =
            self.fast_value_search::<P>(haystack, config, StructuredMode::Yaml, limit)
        {
            return Some(Ok(matches));
        }
        if scan_meets_markers && has_yaml_document_marker(haystack.as_bytes()) {
            return None;
        }
        self.yaml_event_value_search::<P>(haystack, config, limit)
    }

    /// Finds value matches by walking libyaml events, for syntax the byte scanner rejects.
    /// Returns `None` at a key that is a collection, an alias, or a merge, which only the full
    /// parser reads correctly.
    fn yaml_event_value_search<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        config: SearchConfig,
        limit: usize,
    ) -> Option<Result<Vec<StructuredMatch<'a>>, SearchError>> {
        let matcher = self.value_matcher.as_ref().expect("checked by caller");
        let case_insensitive = config.case_mode() == crate::CaseMode::AsciiInsensitive;
        let mut matches = Vec::with_capacity(limit.min(64));
        let mut path_stack: Vec<PathStackEntry> = Vec::with_capacity(32);
        let mut path = Vec::with_capacity(128);
        path.push(b'$');

        let mut parser = YamlParser::new();
        let mut yaml_bytes = haystack.as_bytes();
        parser.set_input_string(&mut yaml_bytes);

        loop {
            let event = match parser.parse() {
                Ok(event) => event,
                Err(error) => {
                    return Some(Err(SearchError::StructuredParse {
                        mode: StructuredMode::Yaml,
                        message: error.to_string(),
                    }));
                }
            };
            let expecting_key = path_stack
                .last()
                .is_some_and(|entry| entry.array_index.is_none() && entry.expecting_key);
            if expecting_key
                && (matches!(
                    event.data,
                    YamlEventData::Alias { .. }
                        | YamlEventData::SequenceStart { .. }
                        | YamlEventData::MappingStart { .. }
                ) || is_yaml_merge_event(&event.data))
            {
                return None;
            }

            match &event.data {
                YamlEventData::StreamEnd => break,
                YamlEventData::Scalar { value, style, .. } => {
                    let start = usize::try_from(event.start_mark.index).unwrap_or(0);
                    let end = usize::try_from(event.end_mark.index).unwrap_or(start);
                    let raw_text = &haystack[start..end];
                    let match_text = match classify_yaml_plain_scalar(raw_text, *style) {
                        PlainYamlScalarKind::Null => "null",
                        PlainYamlScalarKind::Bool(true) => "true",
                        PlainYamlScalarKind::Bool(false) => "false",
                        PlainYamlScalarKind::Number => raw_text,
                        PlainYamlScalarKind::String => value.as_str(),
                    };

                    if !expecting_key
                        && value_matches(matcher, match_text.as_bytes(), config, case_insensitive)
                    {
                        let path_str = P::build(|| {
                            String::from_utf8(path.clone()).expect("path is always valid UTF-8")
                        })
                        .into_string();
                        matches.push(make_structured_match(
                            raw_text,
                            (start, end),
                            0,
                            path_str,
                            StructuredMatchKind::Value,
                        ));
                        if matches.len() == limit {
                            break;
                        }
                    }

                    if let Some(entry) = path_stack.last_mut() {
                        if let Some(index) = entry.array_index.as_mut() {
                            *index += 1;
                            path.truncate(entry.path_len);
                            append_array_segment(&mut path, *index);
                        } else if entry.expecting_key {
                            let path_len_before = entry.path_len;
                            path.truncate(path_len_before);
                            append_key_segment(&mut path, value.as_bytes());
                            entry.expecting_key = false;
                        } else {
                            entry.expecting_key = true;
                        }
                    }
                }
                // An alias is a whole value whose content was already searched.
                YamlEventData::Alias { .. } => {
                    if let Some(entry) = path_stack.last_mut() {
                        if let Some(index) = entry.array_index.as_mut() {
                            *index += 1;
                            path.truncate(entry.path_len);
                            append_array_segment(&mut path, *index);
                        } else {
                            entry.expecting_key = !entry.expecting_key;
                        }
                    }
                }
                YamlEventData::SequenceStart { .. } => {
                    path_stack.push(PathStackEntry {
                        path_len: path.len(),
                        expecting_key: false,
                        array_index: Some(0),
                    });
                    append_array_segment(&mut path, 0);
                }
                YamlEventData::MappingStart { .. } => {
                    path_stack.push(PathStackEntry {
                        path_len: path.len(),
                        expecting_key: true,
                        array_index: None,
                    });
                }
                YamlEventData::SequenceEnd | YamlEventData::MappingEnd => {
                    if let Some(entry) = path_stack.pop() {
                        path.truncate(entry.path_len);
                    }
                    if let Some(parent) = path_stack.last_mut() {
                        if let Some(idx) = parent.array_index.as_mut() {
                            *idx += 1;
                            path.truncate(parent.path_len);
                            append_array_segment(&mut path, *idx);
                        } else {
                            parent.expecting_key = true;
                        }
                    }
                }
                _ => {}
            }
        }
        Some(Ok(matches))
    }
}

/// Returns the value of the YAML scalar written as `raw`, or `None` when only the parser can
/// tell (multi-line, escapes, doubled quotes, block scalars, tags, anchors, aliases).
fn yaml_written_scalar_text(raw: &str) -> Option<&str> {
    if raw.contains('\n') {
        return None;
    }
    match raw.as_bytes().first() {
        Some(b'"') => unquoted(raw, '"').filter(|text| !text.contains('\\')),
        Some(b'\'') => unquoted(raw, '\'').filter(|text| !text.contains('\'')),
        Some(b'|' | b'>' | b'!' | b'&' | b'*') => None,
        Some(_) | None => Some(
            match classify_yaml_plain_scalar(raw, YamlScalarStyle::Plain) {
                PlainYamlScalarKind::Null => "null",
                PlainYamlScalarKind::Bool(true) => "true",
                PlainYamlScalarKind::Bool(false) => "false",
                PlainYamlScalarKind::Number | PlainYamlScalarKind::String => raw,
            },
        ),
    }
}

/// Returns the value of the TOML scalar written as `raw`, or `None` for escaped strings.
fn toml_written_scalar_text(raw: &str) -> Option<&str> {
    match raw.as_bytes().first() {
        Some(b'"') => unquoted(raw, '"').filter(|text| !text.contains('\\')),
        Some(b'\'') => unquoted(raw, '\''),
        Some(_) | None => Some(raw),
    }
}

fn unquoted(raw: &str, quote: char) -> Option<&str> {
    raw.strip_prefix(quote)?.strip_suffix(quote)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_document_markers_are_found_across_scan_blocks() {
        // Markers before, across and after the 128-byte blocks, at the very end, and near
        // misses: two dashes, a dash line, and dashes not at a line start.
        for marker_at in [1_usize, 2, 125, 126, 127, 128, 129, 255, 256, 300, 397] {
            for (marker, found) in [
                (&b"---"[..], true),
                (b"--", false),
                (b"- x", false),
                (b"x---", false),
            ] {
                let mut bytes = vec![b'a'; 400];
                bytes[marker_at - 1] = b'\n';
                let end = (marker_at + marker.len()).min(bytes.len());
                bytes[marker_at..end].copy_from_slice(&marker[..end - marker_at]);
                let found = found && end - marker_at == marker.len();
                assert_eq!(
                    has_newline_marker(&bytes),
                    found,
                    "{marker:?} at {marker_at}"
                );
            }
        }
        assert!(has_yaml_document_marker(b"---\na: 1\n"));
        assert!(!has_newline_marker(b""));
        assert!(!has_newline_marker(b"\n--"));
        assert!(has_newline_marker(b"\n---"));
    }

    #[test]
    fn yaml_streams_hold_one_document_without_a_second_marker_or_content_before_one() {
        let single = [
            "a: 1\n",
            "---\na: 1\n",
            "--- # note\na: 1\n",
            "--- a\n",
            "%YAML 1.2\n---\na: 1\n",
            "# note\n  \n\t# indented\r\n---\na: 1\n",
            "a: x---y\nb:\n  ---\n",
            "a: 1\n...\n",
            "...\n",
        ];
        let several = [
            "a: 1\n---\nb: 2\n",
            "a: 1\r\n---\r\nb: 2\r\n",
            "a: 1\n---\n",
            "---\na: 1\n---\nb: 2\n",
            "  %YAML 1.2\n---\na: 1\n",
            "---x\na: 1\n",
        ];

        for stream in single {
            assert!(is_single_yaml_document(stream.as_bytes()), "{stream:?}");
        }
        for stream in several {
            assert!(!is_single_yaml_document(stream.as_bytes()), "{stream:?}");
        }
    }

    #[test]
    fn written_yaml_scalars_read_as_the_parser_reads_them() {
        let read = [
            ("failed", Some("failed")),
            ("\"failed\"", Some("failed")),
            ("'it works'", Some("it works")),
            ("\"it's\"", Some("it's")),
            ("~", Some("null")),
            ("Null", Some("null")),
            ("True", Some("true")),
            ("off", Some("false")),
            ("1e3", Some("1e3")),
            ("", Some("")),
            ("\"a\\tb\"", None),
            ("\"open", None),
            ("\"", None),
            ("two\n  lines", None),
            ("|", None),
            (">-", None),
            ("*alias", None),
            ("&anchor x", None),
            ("!!str 5", None),
        ];

        for (raw, expected) in read {
            assert_eq!(yaml_written_scalar_text(raw), expected, "{raw:?}");
        }
    }

    #[test]
    fn written_toml_scalars_read_as_the_parser_reads_them() {
        let read = [
            ("\"failed\"", Some("failed")),
            ("'C:\\dir'", Some("C:\\dir")),
            ("42", Some("42")),
            ("true", Some("true")),
            ("\"\"", Some("")),
            ("\"x\\\"y\"", None),
            ("\"open", None),
        ];

        for (raw, expected) in read {
            assert_eq!(toml_written_scalar_text(raw), expected, "{raw:?}");
        }
    }

    #[test]
    fn document_markers_are_found_across_parallel_chunk_boundaries() {
        let line = "key: value-with-dashes - -- ---x\n";
        let mut document = line.repeat(300 * 1024 / line.len());
        assert!(!has_yaml_document_marker(document.as_bytes()));
        assert!(has_yaml_document_marker(b"---\nkey: value\n"));
        assert!(!has_yaml_document_marker(b"--\nkey: value\n"));
        // Straddle the second chunk boundary.
        let boundary = 2 * 64 * 1024;
        document.replace_range(boundary - 1..boundary + 3, "\n---");
        assert!(has_yaml_document_marker(document.as_bytes()));
    }

    #[test]
    fn stream_tasks_are_counted_from_the_stream_length_and_the_pool() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("a small pool starts");

        let counts = pool.install(|| {
            [
                MIN_STREAM_TASK_LEN - 1,
                3 * MIN_STREAM_TASK_LEN,
                1_000 * MIN_STREAM_TASK_LEN,
            ]
            .map(stream_task_count)
        });

        assert_eq!(counts, [1, 3, 16]);
    }

    #[test]
    fn tasks_end_where_the_next_starts_and_the_last_at_the_end() {
        let starts = [3, 9];

        let ranges = [task_range(&starts, 0, 20), task_range(&starts, 1, 20)];

        assert_eq!(ranges, [3..9, 9..20]);
    }

    #[test]
    fn line_starts_spread_tasks_through_a_stream_of_lines() {
        let lines = "{\"n\":1}\n".repeat(4_000);
        // Offsets that fall in one run of whitespace share the start after it.
        let padded = format!("1\n{}2\n{lines}", " \n".repeat(10_000));
        let one_long_line = format!("1\n{}\n3\n", "2 ".repeat(8_000));

        let even = line_value_starts(lines.as_bytes(), 8, 4, 64);
        let after_padding = line_value_starts(padded.as_bytes(), 0, 16, 32_000);
        let within_a_long_line = line_value_starts(one_long_line.as_bytes(), 0, 4, 1_000);
        let past_a_long_line = line_value_starts(one_long_line.as_bytes(), 0, 4, 16_000);

        assert_eq!(even, [8, 8_008, 16_008, 24_008]);
        assert_eq!(after_padding[..3], [0, 20_002, 22_756]);
        assert_eq!(within_a_long_line, [0]);
        assert_eq!(past_a_long_line, [0, 16_003]);
    }

    #[test]
    fn values_of_twice_a_threads_share_of_a_stream_are_outsized() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("a small pool starts");

        let lens = pool.install(|| [0, 1 << 20, 64 << 20].map(outsized_value_len));

        assert_eq!(lens, [JSON_PARALLEL_SCAN_LEN, 1 << 19, 32 << 20]);
    }

    #[test]
    fn short_values_are_read_into_items_of_a_tasks_length() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("a small pool starts");
        // Lines of eight bytes: sixteen tasks' worth, four times the least a task takes.
        let task_len = 4 * MIN_STREAM_TASK_LEN;
        let lines = "{\"n\":1}\n".repeat(16 * task_len / 8);

        let items = pool.install(|| read_stream_items(lines.as_bytes(), 0, usize::MAX));

        let expected =
            (0..16).map(|task| StreamItem::Values(task * task_len..(task + 1) * task_len));
        assert_eq!(items, expected.collect::<Vec<_>>());
    }

    #[test]
    fn outsized_values_are_items_of_their_own_between_the_short_ones() {
        let long = "[1,2,3,4,5,6]";
        let stream = format!("{{\"n\":1}}\n{{\"n\":1}}\n{long}\n{long} {{\"n\":2}}\n{long}\n:");
        let bytes = stream.as_bytes();

        let from_the_start = read_stream_items(bytes, 0, long.len());
        let from_the_last_long_value = read_stream_items(bytes, 52, long.len());
        let short_then_cut = read_stream_items(b"1 2\n:", 0, long.len());
        let nothing = read_stream_items(bytes, bytes.len(), long.len());

        assert_eq!(
            from_the_start,
            [
                StreamItem::Values(0..16),
                StreamItem::Outsized((16, 29)),
                StreamItem::Outsized((30, 43)),
                StreamItem::Values(44..52),
                StreamItem::Outsized((52, 65)),
                // The value that does not parse.
                StreamItem::Values(66..67),
            ]
        );
        assert_eq!(from_the_last_long_value, from_the_start[4..]);
        assert_eq!(short_then_cut, [StreamItem::Values(0..5)]);
        assert_eq!(nothing, []);
    }

    #[test]
    fn task_matches_are_appended_in_order_up_to_the_first_error() {
        let found = |task: usize| {
            let text = ["a", "b", "c", "d"][task];
            vec![make_structured_match(
                text,
                (task, task + 1),
                0,
                String::from("$"),
                StructuredMatchKind::Value,
            )]
        };
        let failure = || SearchError::StructuredParse {
            mode: StructuredMode::Json,
            message: String::from("cut short"),
        };
        let mut every = Vec::new();
        let mut before_the_error = Vec::new();

        let all = append_task_matches(4, |task| Ok(found(task)), &mut every);
        let failed = append_task_matches(
            4,
            |task| {
                if task == 2 {
                    Err(failure())
                } else {
                    Ok(found(task))
                }
            },
            &mut before_the_error,
        );

        assert_eq!(all, Ok(()));
        assert_eq!(every, (0..4).flat_map(found).collect::<Vec<_>>());
        assert_eq!(failed, Err(failure()));
        assert_eq!(before_the_error, (0..2).flat_map(found).collect::<Vec<_>>());
    }

    #[test]
    fn parts_that_end_early_stop_before_values_left_to_a_search_in_order() {
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .build();
        let engine = StructuredEngine::new(&[b"$.n > 1".to_vec()], config).expect("valid pattern");
        let long = format!("{{\"n\":[{}2]}}", "1,".repeat(JSON_PARALLEL_SCAN_LEN / 2));
        let stream = format!("{{\"n\":2}}\n{{\"n\":3}}\n{long}\n{{\"n\":4}}\n:");
        let long_start = 16;
        let search = |values: Range<usize>, stops_before| {
            let part = StreamPart {
                values,
                stops_before,
            };
            let (found, next) = engine
                .search_stream_part::<String>(&stream, part, config, usize::MAX)
                .expect("values should be searched");
            let starts = found.iter().map(|matched| matched.start);
            (starts.collect::<Vec<_>>(), next)
        };
        let cut = stream.len() - 1;

        let up_to_the_long_value = search(0..stream.len(), Some(JSON_PARALLEL_SCAN_LEN));
        let first_value = search(0..8, None);
        let up_to_the_cut_value = search(cut - 8..stream.len(), Some(JSON_PARALLEL_SCAN_LEN));
        let every_value = search(0..stream.len(), None);
        let whole_stream = engine.search(&stream, config, usize::MAX);

        assert_eq!(up_to_the_long_value, (vec![5, 13], long_start));
        assert_eq!(first_value, (vec![5], 8));
        assert_eq!(up_to_the_cut_value, (vec![cut - 3], cut));
        // The long value's numbers sit in an array, which the pattern's path does not name.
        assert_eq!(every_value, (vec![5, 13, cut - 3], stream.len()));
        assert_eq!(whole_stream.map(|found| found.len()), Ok(3));
    }

    #[test]
    fn unlimited_yaml_value_scans_leave_multi_document_streams_to_the_ast() {
        // The marker follows skipped items, or (last stream) syntax the scanner rejects.
        let filler = "- name: skipped\n  note: nothing here\n".repeat(40);
        let skipped_items = format!("{filler}---\n- name: needle\n");
        let chunked = format!(
            "{}---\n- name: needle\n{}",
            filler.repeat(120),
            filler.repeat(60)
        );
        let flow_syntax = "tags:\n  [needle, other]\n---\nname: needle\n";
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Yaml))
            .build();
        let engine = StructuredEngine::new(&[b"needle".to_vec()], config).expect("valid pattern");

        assert!(chunked.len() > 256 * 1024);
        for stream in [skipped_items.as_str(), chunked.as_str(), flow_syntax] {
            for limit in [1, usize::MAX] {
                assert!(
                    engine
                        .fast_search::<String>(stream, config, StructuredMode::Yaml, limit)
                        .is_none(),
                    "limit {limit}, stream of {} bytes",
                    stream.len()
                );
            }
        }
        let single_document = skipped_items.replace("---\n", "");
        let matches = engine
            .search(single_document.as_str(), config, usize::MAX)
            .expect("valid YAML");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "$[40].name");
    }

    #[test]
    fn shared_dispatch_preserves_format_specific_values_paths_and_predicates() {
        let count_path = "$.events[0].count";
        let name_path = "$.events[0].name";
        for (mode, document) in [
            (
                StructuredMode::Json,
                r#"{"events":[{"name":"Alice","count":2}]}"#,
            ),
            (
                StructuredMode::Yaml,
                "events:\n  - name: \"Alice\"\n    count: 2\n",
            ),
            // TOML direct navigation supports arrays of tables, not root scalar keys.
            (
                StructuredMode::Toml,
                "[[events]]\nname = \"Alice\"\ncount = 2\n",
            ),
        ] {
            let config = SearchConfig::builder().structured_mode(Some(mode)).build();
            for (pattern, kind, path) in [
                (count_path, StructuredMatchKind::Path, count_path),
                (
                    "$.events[0].count=2",
                    StructuredMatchKind::PathValue,
                    count_path,
                ),
                ("Alice", StructuredMatchKind::Value, name_path),
                ("$.count > 1", StructuredMatchKind::Predicate, count_path),
                (
                    "$.count > 1 AND $.name=\"Alice\"",
                    StructuredMatchKind::Predicate,
                    count_path,
                ),
                (
                    "$.count > 1 AND $.count < 3",
                    StructuredMatchKind::Predicate,
                    count_path,
                ),
            ] {
                let engine = StructuredEngine::new(&[pattern.as_bytes().to_vec()], config)
                    .expect("valid pattern");
                let matches = engine.search(document, config, 1).expect("valid document");
                assert_eq!(matches.len(), 1, "{mode:?}: {pattern}");
                assert_eq!(matches[0].path, path);
                assert_eq!(matches[0].match_kind, kind);
                assert_eq!(
                    matches[0].node_text,
                    &document[matches[0].start..matches[0].end]
                );
            }
            let engine = StructuredEngine::new(&[b"$.events[0].name=Alice".to_vec()], config)
                .expect("valid path-value query");
            let matches = engine.search(document, config, 1).expect("valid document");
            // Quotes around the document's value are not part of it.
            assert_eq!(matches.len(), 1, "{mode:?}");
            let mixed =
                StructuredEngine::new(&[b"$.events[0].count".to_vec(), b"Alice".to_vec()], config)
                    .expect("valid mixed queries");
            let matches = mixed
                .search(document, config, usize::MAX)
                .expect("valid document");
            assert_eq!(
                matches
                    .iter()
                    .map(|matched| matched.path.as_str())
                    .collect::<Vec<_>>(),
                vec![name_path, count_path]
            );
        }
    }

    #[test]
    fn shared_collection_keeps_query_order_limits_and_mixed_query_fallback() {
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .overlapping(true)
            .build();
        let engine = StructuredEngine::new(&[b"$.b".to_vec(), b"$.a".to_vec()], config)
            .expect("valid paths");
        let matches = engine
            .search(r#"{"a":1,"b":2}"#, config, 1)
            .expect("valid JSON");
        assert_eq!(matches[0].path, "$.b");
        assert_eq!(matches[0].pattern_index, 0);
        let mixed =
            StructuredEngine::new(&[b"$.a".to_vec(), b"1".to_vec(), b"11".to_vec()], config)
                .expect("valid mixed patterns");
        assert!(mixed.fast_query_kind().is_none());
        assert!(
            mixed
                .fast_search::<String>(r#"{"a":1}"#, config, StructuredMode::Json, 1)
                .is_none()
        );
        let matches = mixed
            .search(r#"{"a":11}"#, config, usize::MAX)
            .expect("valid JSON");
        assert_eq!(matches.len(), 3);
        for (index, matched) in matches.iter().enumerate() {
            assert_eq!(matched.pattern_index, index);
            assert_eq!(matched.node_text, "11");
            assert_eq!(matched.node_text.as_ptr(), matches[0].node_text.as_ptr());
        }
        assert!(matches!(
            StructuredEngine::new(&[b"$.a > true".to_vec()], config),
            Err(SearchError::InvalidStructuredPattern { index: 0, .. })
        ));
    }
}
