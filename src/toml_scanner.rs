use std::ops::Range;

use hashbrown::HashMap;
use memchr::{memchr, memchr_iter, memchr2, memrchr};

use crate::chunks::map_chunks;
use crate::jq_path::{append_array_segment, append_dotted_key_span};
use crate::json_scanner::collection::{HitPath, collect_hits, push_hit};
use crate::occurrences::Needle;
use crate::profiles;
use crate::yaml_scanner::lines::{
    ContentLine, next_line as find_line_end, next_line_content, skip_inline_whitespace,
};
use crate::yaml_scanner::scalar::scalar_extent;

mod navigation;

pub(crate) use navigation::{TomlPath, toml_navigate_path};

#[cfg(test)]
mod tests;

/// Smallest input scanned as parallel line-aligned chunks.
const PARALLEL_SCAN_LEN: usize = 64 * 1024;
/// Smallest chunk handed to one parallel scan task.
const MIN_CHUNK_LEN: usize = 16 * 1024;
/// Chunks per rayon thread, so work stealing can balance uneven chunks.
const CHUNKS_PER_THREAD: usize = 4;

/// Returns the `(start, end)` span of the TOML value at `start`.
fn extract_toml_value(bytes: &[u8], start: usize) -> (usize, usize) {
    scalar_extent(bytes, start, || {
        memchr2(b'\n', b'#', &bytes[start..]).map_or(bytes.len(), |offset| start + offset)
    })
}

/// A matched TOML scalar and its path.
pub(crate) use crate::json_scanner::collection::ScalarHit as TomlScalarHit;

/// Returns up to `limit` scalar values for which `matcher` returns true, or `None` when the
/// document needs the full parser.
pub(crate) fn toml_scan_values<F, P>(
    bytes: &[u8],
    limit: usize,
    matcher: F,
) -> Option<Vec<TomlScalarHit<P>>>
where
    F: Fn(&[u8]) -> bool + Sync,
    P: HitPath,
{
    scan_toml_hits(
        bytes,
        limit,
        |start, end| matcher(&bytes[start..end]),
        &TomlLines::All,
    )
}

/// Folds every scalar into `init()` states, one per parallel line-aligned chunk, returned in
/// document order. Returns `None` when the document needs the full parser.
pub(crate) fn toml_fold_values_with_paths<S, I, V>(
    bytes: &[u8],
    init: I,
    visit: V,
) -> Option<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    V: Fn(&mut S, &[u8], usize, usize) + Sync,
{
    map_toml_chunks(bytes, |range, mut state| {
        let mut folded = init();
        toml_traverse_values(bytes, range, &mut state, |state, key, start, end| {
            visit(&mut folded, state.key_path(bytes, key), start, end);
            true
        })?;
        Some(folded)
    })
}

/// Like [`toml_scan_values`], but only parses key-value lines that contain `needle`.
pub(crate) fn toml_scan_values_prefiltered<F, P>(
    bytes: &[u8],
    needle: &Needle,
    limit: usize,
    matcher: F,
) -> Option<Vec<TomlScalarHit<P>>>
where
    F: Fn(&[u8]) -> bool + Sync,
    P: HitPath,
{
    // A needle spanning a newline belongs to no single line, so every line is visited.
    let lines = if needle.as_bytes().contains(&b'\n') {
        TomlLines::All
    } else {
        TomlLines::Containing(needle)
    };
    scan_toml_hits(
        bytes,
        limit,
        |start, end| start < end && matcher(&bytes[start..end]),
        &lines,
    )
}

/// The lines whose values a TOML scan reads; header lines always update the table path.
#[derive(Debug)]
enum TomlLines<'a> {
    /// Every content line.
    All,
    /// Lines holding an occurrence of the needle.
    Containing(&'a Needle),
}

fn scan_toml_hits<F, P>(
    bytes: &[u8],
    limit: usize,
    accepts: F,
    lines: &TomlLines<'_>,
) -> Option<Vec<TomlScalarHit<P>>>
where
    F: Fn(usize, usize) -> bool + Sync,
    P: HitPath,
{
    if limit == usize::MAX {
        collect_toml_hits::<false, _, _>(bytes, usize::MAX, &accepts, lines)
    } else {
        collect_toml_hits::<true, _, _>(bytes, limit, &accepts, lines)
    }
}

// `LIMITED` is const so unlimited scans skip the hit-count check entirely.
fn collect_toml_hits<const LIMITED: bool, F, P>(
    bytes: &[u8],
    limit: usize,
    accepts: &F,
    lines: &TomlLines<'_>,
) -> Option<Vec<TomlScalarHit<P>>>
where
    F: Fn(usize, usize) -> bool + Sync,
    P: HitPath,
{
    collect_hits(
        (bytes.len(), limit),
        TomlScanState::new,
        |range, state| {
            let mut hits = Vec::new();
            let end = range.end;
            let callback = |state: &mut TomlScanState<'_>, key: (usize, usize), start, end| {
                let (text_start, text_end) = unquoted_span(bytes, start, end);
                if accepts(text_start, text_end) {
                    push_hit(&mut hits, state.key_path(bytes, key), start, end);
                }
                !LIMITED || hits.len() < limit
            };
            let chunk_start = range.start;
            let read = match lines {
                TomlLines::All => toml_traverse_values(bytes, range, state, callback),
                TomlLines::Containing(needle) => {
                    let occurrences = needle
                        .find_iter(&bytes[range.clone()])
                        .map(|offset| chunk_start + offset);
                    toml_traverse_occurrences(bytes, occurrences, range, state, callback)
                }
            };
            // Skipped lines could open a multi-line string, which makes later lines its text.
            // Only an opener before the last visited line can change the hits.
            let visited_end = match hits.last() {
                Some(last) if LIMITED && hits.len() >= limit => last.end,
                _ => end,
            };
            let skipped_string = matches!(lines, TomlLines::Containing(_))
                && contains_triple_quote(&bytes[chunk_start..visited_end]);
            (hits, read.filter(|()| !skipped_string).map(|()| end))
        },
        |scan| {
            map_toml_chunks(bytes, |range, mut state| {
                let (hits, reached) = scan(range, &mut state);
                reached.map(|_| hits)
            })
        },
    )
}

/// The scanner's state at a line start: the table path and array-of-tables header counts.
#[derive(Clone)]
struct TomlScanState<'a> {
    table_path: Vec<u8>,
    table_path_len: usize,
    array_counts: ArrayCounts<'a>,
}

/// Header counts per array-of-tables name, as spelled in the document.
type ArrayCounts<'a> = HashMap<&'a [u8], usize>;

impl TomlScanState<'_> {
    fn new() -> Self {
        let mut table_path = Vec::with_capacity(64);
        table_path.push(b'$');
        Self {
            table_path,
            table_path_len: 1,
            array_counts: HashMap::new(),
        }
    }

    /// The path of the key at `bytes[key.0..key.1]` in the current table.
    #[inline]
    fn key_path(&mut self, bytes: &[u8], (start, end): (usize, usize)) -> &[u8] {
        self.table_path.truncate(self.table_path_len);
        append_dotted_key_span(&mut self.table_path, bytes, start, end);
        &self.table_path
    }
}

/// One chunk's headers, summarized so chunk start states compose without replaying them.
struct TomlHeaderSummary<'a> {
    /// Array-of-tables names with their header counts within the chunk.
    array_counts: ArrayCounts<'a>,
    /// Names of the chunk's headers with dotted names.
    dotted_names: Vec<&'a [u8]>,
    /// Position of the `[` opening the chunk's last header line.
    last_header: Option<usize>,
}

/// Maps `scan` over line-aligned chunks in parallel, each with the scanner state at its first
/// line, and returns the results in document order, or `None` if any chunk needs the parser.
fn map_toml_chunks<'a, T, F>(bytes: &'a [u8], scan: F) -> Option<Vec<T>>
where
    T: Send,
    F: Fn(Range<usize>, TomlScanState<'a>) -> Option<T> + Sync,
{
    let chunk_count = if bytes.len() < PARALLEL_SCAN_LEN || !profiles::has_parallel_workers() {
        1
    } else {
        (bytes.len() / MIN_CHUNK_LEN).clamp(1, CHUNKS_PER_THREAD * rayon::current_num_threads())
    };
    if chunk_count == 1 {
        return Some(vec![scan(0..bytes.len(), TomlScanState::new())?]);
    }

    let mut bounds = Vec::with_capacity(chunk_count + 1);
    bounds.push(0);
    for chunk_index in 1..chunk_count {
        let target = bytes.len() * chunk_index / chunk_count;
        let bound =
            memchr(b'\n', &bytes[target..]).map_or(bytes.len(), |offset| target + offset + 1);
        if bound > bounds[bounds.len() - 1] && bound < bytes.len() {
            bounds.push(bound);
        }
    }
    bounds.push(bytes.len());

    let summaries = map_chunks(bounds.len() - 1, |chunk| {
        summarize_toml_headers(bytes, bounds[chunk]..bounds[chunk + 1])
    });
    let mut state = TomlScanState::new();
    let mut chunks = Vec::with_capacity(summaries.len());
    for (window, summary) in bounds.windows(2).zip(summaries) {
        // Copy only the counts this chunk's headers use; copying all of them per chunk could
        // take many times the document's size.
        let array_counts = summary
            .array_counts
            .keys()
            .map(|&name| (name, state.array_counts.get(name).copied().unwrap_or(0)))
            .collect();
        chunks.push((
            window[0]..window[1],
            TomlScanState {
                table_path: state.table_path.clone(),
                table_path_len: state.table_path_len,
                array_counts,
            },
        ));
        for (name, count) in summary.array_counts {
            *state.array_counts.entry(name).or_default() += count;
        }
        // A chunk's own scan sees only the arrays its headers name, so arrays named in earlier
        // chunks are checked here.
        if summary
            .dotted_names
            .iter()
            .any(|name| crosses_array_of_tables(name, &state.array_counts))
        {
            return None;
        }
        if let Some(position) = summary.last_header {
            // Counts already include this header; only restore its table path.
            update_toml_table_path(bytes, position, &mut state, 0)?;
        }
    }
    map_chunks(chunks.len(), |chunk| {
        let (range, state) = &chunks[chunk];
        scan(range.clone(), state.clone())
    })
    .into_iter()
    .collect()
}

fn summarize_toml_headers(bytes: &[u8], range: Range<usize>) -> TomlHeaderSummary<'_> {
    let mut summary = TomlHeaderSummary {
        array_counts: HashMap::new(),
        dotted_names: Vec::new(),
        last_header: None,
    };
    for position in toml_header_positions(bytes, range) {
        let (is_array, start, end) = toml_header_name(bytes, position);
        let name = &bytes[start..end];
        if is_array {
            *summary.array_counts.entry(name).or_default() += 1;
        }
        if name.contains(&b'.') {
            summary.dotted_names.push(name);
        }
        summary.last_header = Some(position);
    }
    summary
}

/// Yields the position of the `[` opening each header line in the line-aligned `range`.
fn toml_header_positions(bytes: &[u8], range: Range<usize>) -> impl Iterator<Item = usize> {
    memchr_iter(b'[', &bytes[range.clone()])
        .map(move |offset| range.start + offset)
        .filter(move |&position| {
            let mut line_start = position;
            while line_start > range.start && matches!(bytes[line_start - 1], b' ' | b'\t') {
                line_start -= 1;
            }
            line_start == range.start || bytes[line_start - 1] == b'\n'
        })
}

/// Walks the lines of `bytes[range]`, which must begin at a line start. Returns `None` at a
/// line only the full parser reads correctly.
fn toml_traverse_values<'a, C>(
    bytes: &'a [u8],
    mut range: Range<usize>,
    state: &mut TomlScanState<'a>,
    mut callback: C,
) -> Option<()>
where
    C: FnMut(&mut TomlScanState<'a>, (usize, usize), usize, usize) -> bool,
{
    while let Some(line) = next_line_content::<true>(bytes, &mut range) {
        if !visit_toml_line(bytes, line, state, &mut callback)? {
            break;
        }
    }
    Some(())
}

/// Like [`toml_traverse_values`], but only parses lines containing one of the ascending
/// `occurrences`; header lines in between still update `state`.
fn toml_traverse_occurrences<'a, C>(
    bytes: &'a [u8],
    occurrences: impl Iterator<Item = usize>,
    mut range: Range<usize>,
    state: &mut TomlScanState<'a>,
    mut callback: C,
) -> Option<()>
where
    C: FnMut(&mut TomlScanState<'a>, (usize, usize), usize, usize) -> bool,
{
    for occurrence in occurrences {
        if occurrence < range.start {
            continue;
        }
        let line_start = memrchr(b'\n', &bytes[range.start..occurrence])
            .map_or(range.start, |newline| range.start + newline + 1);
        for header in toml_header_positions(bytes, range.start..line_start) {
            update_toml_table_path(bytes, header, state, 1)?;
        }
        let mut line_range = line_start..find_line_end(bytes, line_start).min(range.end);
        range.start = line_range.end;
        if let Some(line) = next_line_content::<true>(bytes, &mut line_range)
            && !visit_toml_line(bytes, line, state, &mut callback)?
        {
            break;
        }
    }
    Some(())
}

/// Applies a header line to `state` or reports a key-value line's span through `callback`.
/// Returns `Some(false)` when `callback` asks to stop, and `None` for a line the scanner
/// doesn't model: a quoted or spaced key, an inline table or array, a multi-line string, or a
/// line continuing one of them.
#[inline(always)]
fn visit_toml_line<'a, C>(
    bytes: &'a [u8],
    line: ContentLine,
    state: &mut TomlScanState<'a>,
    callback: &mut C,
) -> Option<bool>
where
    C: FnMut(&mut TomlScanState<'a>, (usize, usize), usize, usize) -> bool,
{
    if bytes[line.content] == b'[' {
        update_toml_table_path(bytes, line.content, state, 1)?;
        return Some(true);
    }
    let (key, value_start) = parse_key_value_line(bytes, line.content, line.end)?;
    let value = &bytes[value_start..line.end];
    if is_quoted_or_spaced(key)
        || matches!(value.first(), Some(b'[' | b'{'))
        || value.starts_with(b"\"\"\"")
        || value.starts_with(b"'''")
    {
        return None;
    }
    let (value_start, value_end) = extract_toml_value(bytes, value_start);
    let key_span = (line.content, line.content + key.len());
    Some(callback(state, key_span, value_start, value_end))
}

/// Whether a key holds a byte that bare keys can't, such as a quote or whitespace, so the
/// scanner would write it into a path wrongly. Those bytes all sort below `-`, the lowest byte
/// of bare and dotted keys, so one branch-free minimum replaces per-byte comparisons; it runs
/// on every key-value line.
fn is_quoted_or_spaced(key: &[u8]) -> bool {
    key.iter().fold(u8::MAX, |lowest, &byte| lowest.min(byte)) < b'-'
}

/// Whether `bytes` holds `"""` or `'''`. Quotes are common in TOML, which would make `memmem`
/// verify a candidate at nearly every string, so this compares shifted blocks branch-free.
fn contains_triple_quote(bytes: &[u8]) -> bool {
    const BLOCK_LEN: usize = 1024;
    let Some(windows) = bytes.len().checked_sub(2) else {
        return false;
    };
    let shifted = |by: usize| bytes[by..by + windows].chunks(BLOCK_LEN);
    shifted(0)
        .zip(shifted(1))
        .zip(shifted(2))
        .any(|((first, second), third)| {
            first
                .iter()
                .zip(second)
                .zip(third)
                .fold(false, |found, ((&a, &b), &c)| {
                    found | ((a == b) & (b == c) & ((a == b'"') | (a == b'\'')))
                })
        })
}

/// Whether `name` is bare keys joined by dots, which the scanner writes into paths as is.
fn is_bare_dotted_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Whether a dotted header `name` continues an array of tables, whose elements the scanner
/// doesn't index in sub-table paths.
fn crosses_array_of_tables(name: &[u8], array_counts: &ArrayCounts<'_>) -> bool {
    memchr_iter(b'.', name).any(|dot| array_counts.contains_key(&name[..dot]))
}

/// Returns whether the header at `pos` is an array of tables, and its name's byte range.
fn toml_header_name(bytes: &[u8], pos: usize) -> (bool, usize, usize) {
    let is_array = pos + 1 < bytes.len() && bytes[pos + 1] == b'[';
    let header_start = pos + usize::from(is_array) + 1;
    // Bound the `]` search to the line, or unclosed headers would make scanning quadratic.
    let line_end = find_line_end(bytes, header_start);
    let header_end = memchr(b']', &bytes[header_start..line_end])
        .map_or(line_end, |offset| header_start + offset);
    (is_array, header_start, header_end)
}

/// Applies the header at `pos` to `state`, adding `added` to its array count (zero when the
/// chunk summary already counted it). Returns `None` for a header the scanner doesn't model.
fn update_toml_table_path<'a>(
    bytes: &'a [u8],
    pos: usize,
    state: &mut TomlScanState<'a>,
    added: usize,
) -> Option<()> {
    let (is_array, start, end) = toml_header_name(bytes, pos);
    let name = &bytes[start..end];
    if !is_bare_dotted_name(name) || crosses_array_of_tables(name, &state.array_counts) {
        return None;
    }
    let index = is_array.then(|| {
        let count = state.array_counts.entry(&bytes[start..end]).or_default();
        *count += added;
        *count - 1
    });
    state.table_path.truncate(1);
    append_dotted_key_span(&mut state.table_path, bytes, start, end);
    if let Some(index) = index {
        append_array_segment(&mut state.table_path, index);
    }
    state.table_path_len = state.table_path.len();
    Some(())
}

/// The span of a scalar's text: a quoted string's span without its quotes.
fn unquoted_span(bytes: &[u8], start: usize, end: usize) -> (usize, usize) {
    if end - start >= 2 && matches!(bytes[start], b'"' | b'\'') && bytes[end - 1] == bytes[start] {
        (start + 1, end - 1)
    } else {
        (start, end)
    }
}

/// Parses a key-value line into `(key, value_start)`.
fn parse_key_value_line(bytes: &[u8], pos: usize, line_end: usize) -> Option<(&[u8], usize)> {
    let eq_pos = pos + memchr(b'=', &bytes[pos..line_end])?;

    let mut key_end = eq_pos;
    while key_end > pos && matches!(bytes[key_end - 1], b' ' | b'\t') {
        key_end -= 1;
    }

    // Cut at ASCII delimiters of a `&str`, so the key stays valid UTF-8.
    let key = &bytes[pos..key_end];

    let value_start = skip_inline_whitespace::<true>(bytes, eq_pos + 1);

    Some((key, value_start))
}
