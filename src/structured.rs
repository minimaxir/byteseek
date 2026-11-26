use hashbrown::HashMap;
use json_spanned_value::spanned as json_spanned;
use smallvec::SmallVec;

use self::collector::MatchCollector;
use self::path::format_path_from_segments;
use crate::exact::{MultiPatternEngine, SinglePatternEngine};
use crate::json_scanner::{
    HitPath, JsonPathBatch, ParsedPathQuery, PathSegment, json_stream_extent, parse_path_pattern,
};
use crate::occurrences::Needle;
use crate::structured_predicate::{
    PredicateLiteral, PredicateOperator, PredicatePattern, is_predicate_pattern,
    parse_predicate_pattern,
};
use crate::toml_scanner::TomlPath;
use crate::{
    BYTE_ORDER_MARK, CaseMode, SearchConfig, SearchError, StructuredMatch, StructuredMode,
    normalize_pattern,
};

mod collector;
mod fast;
mod objects;
mod path;
mod predicate;
#[cfg(feature = "python")]
pub(crate) mod python_value;
mod value;

/// Pattern indexes keyed by the node text (path, or `path=value`) they name. Most node texts
/// have a length no name has, which rejects them before hashing.
#[derive(Default)]
struct PatternsByText {
    patterns: HashMap<Vec<u8>, SmallVec<[usize; 4]>>,
    /// Whether some name is as long as the index.
    lengths: Vec<bool>,
}

impl PatternsByText {
    /// Records that `pattern_index` names `text`. Patterns are recorded both as written and
    /// in canonical form, which differ for needlessly quoted keys.
    fn name(&mut self, text: Vec<u8>, pattern_index: usize) {
        if self.lengths.len() <= text.len() {
            self.lengths.resize(text.len() + 1, false);
        }
        self.lengths[text.len()] = true;
        let patterns = self.patterns.entry(text).or_default();
        if patterns.last() != Some(&pattern_index) {
            patterns.push(pattern_index);
        }
    }

    /// Whether some name is `len` bytes long.
    fn names_length(&self, len: usize) -> bool {
        self.lengths.get(len).copied().unwrap_or(false)
    }

    /// The patterns naming `text`, in pattern order. A few names are compared directly,
    /// from the last byte, where paths of the same length usually differ, which beats
    /// hashing the whole text.
    fn get(&self, text: &[u8]) -> Option<&SmallVec<[usize; 4]>> {
        const MOST_COMPARED: usize = 8;
        if !self.names_length(text.len()) {
            return None;
        }
        if self.patterns.len() > MOST_COMPARED {
            return self.patterns.get(text);
        }
        self.patterns
            .iter()
            .find(|(name, _)| name.last() == text.last() && name.as_slice() == text)
            .map(|(_, patterns)| patterns)
    }
}

pub(crate) struct StructuredEngine {
    /// Path patterns by exact node path, so parsed documents match only that node, as the
    /// scanners do, rather than every path containing the pattern text.
    nodes_by_path: PatternsByText,
    /// `path=value` patterns by exact node path and value.
    nodes_by_path_value: PatternsByText,
    path_matcher: Option<PatternMatcher>,
    value_matcher: Option<PatternMatcher>,
    path_value_matcher: Option<PatternMatcher>,
    compiled_predicates: Vec<CompiledPredicatePattern>,
    /// Whether some predicate clause's path suffix ends with the index byte; one lookup
    /// rejects most scalar paths.
    predicate_suffix_last_bytes: [bool; 256],
    /// Text every sequence item holding a predicate match must contain.
    predicate_item_needle: Option<Needle>,
    /// Path queries for direct navigation without a full parse.
    parsed_path_queries: Vec<ParsedPathQuery>,
    json_path_batch: Option<JsonPathBatch>,
    /// Path-value queries for direct navigation plus a value check.
    parsed_path_value_queries: Vec<ParsedPathQuery>,
    /// TOML navigation paths per path query (`None` if not navigable); empty unless TOML.
    toml_paths: Vec<Option<TomlPath>>,
    /// TOML navigation paths per path-value query.
    toml_path_value_paths: Vec<Option<TomlPath>>,
}

enum ExactStructuredEngine {
    Single(Box<SinglePatternEngine>),
    Multi(Box<MultiPatternEngine>),
}

struct PatternMatcher {
    engine: ExactStructuredEngine,
    /// Whole-document finder for a single pattern, used to skip regions without it. Built
    /// once, since building it per search dominated first-match searches.
    prefilter_needle: Option<Needle>,
    pattern_lengths: Vec<usize>,
    pattern_indexes: Vec<usize>,
}

struct CompiledPredicatePattern {
    pattern_index: usize,
    clauses: Vec<CompiledPredicateClause>,
}

struct CompiledPredicateClause {
    clause_index: usize,
    segments: Vec<PathSegment>,
    suffix_bytes: Vec<u8>,
    operator: crate::structured_predicate::PredicateOperator,
}

/// A pattern sorted by kind. Path kinds carry their parsed query when the path parses;
/// either way their exact text still names nodes.
enum StructuredPatternKind {
    Path(Option<ParsedPathQuery>),
    Value,
    PathValue(Option<ParsedPathQuery>),
    Predicate,
}

fn compile_predicate_pattern(predicate: PredicatePattern) -> CompiledPredicatePattern {
    let clauses = predicate
        .clauses
        .into_iter()
        .enumerate()
        .map(|(clause_index, clause)| {
            let mut suffix = format_path_from_segments(&clause.segments);
            suffix.remove(0);
            CompiledPredicateClause {
                clause_index,
                segments: clause.segments,
                suffix_bytes: suffix.into_bytes(),
                operator: clause.operator,
            }
        })
        .collect();

    CompiledPredicatePattern {
        pattern_index: predicate.pattern_index,
        clauses,
    }
}

/// Returns the longest string the only predicate pattern requires some value to equal.
///
/// Only applies when no clause indexes a sequence, so all clauses read one object, and any
/// sequence item lacking the string cannot match.
fn predicate_item_needle(predicates: &[CompiledPredicatePattern]) -> Option<Needle> {
    // Slice pattern: matches exactly one element.
    let [pattern] = predicates else {
        return None;
    };
    let reads_one_object = pattern.clauses.iter().all(|clause| {
        clause
            .segments
            .iter()
            .all(|segment| matches!(segment, PathSegment::Key(_)))
    });
    if !reads_one_object {
        return None;
    }
    pattern
        .clauses
        .iter()
        .filter_map(|clause| match &clause.operator {
            PredicateOperator::Eq(PredicateLiteral::String(text)) => Some(text.as_bytes()),
            PredicateOperator::Eq(
                PredicateLiteral::Null
                | PredicateLiteral::Bool(_)
                | PredicateLiteral::Number(_)
                | PredicateLiteral::Timestamp(_),
            )
            | PredicateOperator::NotEq(_)
            | PredicateOperator::GreaterThan(_)
            | PredicateOperator::GreaterThanOrEqual(_)
            | PredicateOperator::LessThan(_)
            | PredicateOperator::LessThanOrEqual(_)
            | PredicateOperator::Between { .. } => None,
        })
        .filter(|text| text.len() >= 2 && !text.contains(&b'\n'))
        .max_by_key(|text| text.len())
        .map(|text| Needle::new(text, false))
}

/// Stack kept free below each document search and each container visit. Scanners and
/// parsers recurse per nesting level, needing up to 512 KiB at their depth limits in debug
/// builds and a quarter of that in release builds; TOML dotted keys nest the visit without
/// any limit.
const STACK_RED_ZONE: usize = 1 << 20;
/// Heap stack segment allocated when less than [`STACK_RED_ZONE`] remains. Pages are only
/// committed as they are touched.
const STACK_SEGMENT: usize = 4 << 20;

/// Runs `work` with at least [`STACK_RED_ZONE`] bytes of stack, moving it onto a new heap
/// segment when the current thread's stack runs low, so deeply nested documents can't
/// overflow small thread stacks.
pub(crate) fn with_stack_room<R>(work: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(STACK_RED_ZONE, STACK_SEGMENT, work)
}

impl StructuredEngine {
    pub(crate) fn new(patterns: &[Vec<u8>], config: SearchConfig) -> Result<Self, SearchError> {
        let mut path_patterns = Vec::new();
        let mut value_patterns = Vec::new();
        let mut path_value_patterns = Vec::new();
        let mut compiled_predicates = Vec::new();
        let mut parsed_path_queries = Vec::new();
        let mut parsed_path_value_queries = Vec::new();
        let mut nodes_by_path = PatternsByText::default();
        let mut nodes_by_path_value = PatternsByText::default();

        for (pattern_index, pattern) in patterns.iter().enumerate() {
            match classify_structured_pattern(pattern, pattern_index) {
                StructuredPatternKind::Path(query) => {
                    path_patterns.push((pattern_index, pattern.clone()));
                    nodes_by_path.name(pattern.clone(), pattern_index);
                    if let Some(query) = query {
                        let path = format_path_from_segments(&query.segments);
                        nodes_by_path.name(path.into_bytes(), pattern_index);
                        parsed_path_queries.push(query);
                    }
                }
                StructuredPatternKind::Value => {
                    value_patterns.push((pattern_index, pattern.clone()));
                }
                StructuredPatternKind::PathValue(query) => {
                    path_value_patterns.push((pattern_index, pattern.clone()));
                    nodes_by_path_value.name(pattern.clone(), pattern_index);
                    if let Some(query) = query {
                        let path = format_path_from_segments(&query.segments);
                        let value = query.expected_value.as_deref().unwrap_or_default();
                        let named = format!("{path}={value}").into_bytes();
                        nodes_by_path_value.name(named, pattern_index);
                        parsed_path_value_queries.push(query);
                    }
                }
                StructuredPatternKind::Predicate => {
                    let text = std::str::from_utf8(pattern).map_err(|error| {
                        SearchError::InvalidStructuredPattern {
                            index: pattern_index,
                            message: error.to_string(),
                        }
                    })?;
                    let predicate = parse_predicate_pattern(text, pattern_index)?;
                    compiled_predicates.push(compile_predicate_pattern(predicate));
                }
            }
        }

        let json_path_batch = (parsed_path_queries.len() >= 8
            && config
                .max_matches()
                .is_none_or(|limit| limit >= parsed_path_queries.len())
            && config.structured_mode() == Some(StructuredMode::Json)
            && value_patterns.is_empty()
            && path_value_patterns.is_empty()
            && compiled_predicates.is_empty())
        .then(|| JsonPathBatch::new(&parsed_path_queries));

        let navigates_toml = config.structured_mode() == Some(StructuredMode::Toml);
        let toml_paths_of = |queries: &[ParsedPathQuery]| -> Vec<Option<TomlPath>> {
            if !navigates_toml {
                return Vec::new();
            }
            let paths = queries.iter().map(|query| TomlPath::new(&query.segments));
            paths.collect()
        };
        let toml_paths = toml_paths_of(&parsed_path_queries);
        let toml_path_value_paths = toml_paths_of(&parsed_path_value_queries);

        let mut predicate_suffix_last_bytes = [false; 256];
        for clause in compiled_predicates
            .iter()
            .flat_map(|pattern| &pattern.clauses)
        {
            if let Some(&last) = clause.suffix_bytes.last() {
                predicate_suffix_last_bytes[usize::from(last)] = true;
            }
        }

        Ok(Self {
            nodes_by_path,
            nodes_by_path_value,
            json_path_batch,
            predicate_item_needle: predicate_item_needle(&compiled_predicates),
            predicate_suffix_last_bytes,
            path_matcher: PatternMatcher::new(path_patterns, config)?,
            value_matcher: PatternMatcher::new(value_patterns, config)?,
            path_value_matcher: PatternMatcher::new(path_value_patterns, config)?,
            compiled_predicates,
            parsed_path_queries,
            parsed_path_value_queries,
            toml_paths,
            toml_path_value_paths,
        })
    }

    /// Finds structured matches with their paths.
    pub(crate) fn search<'a>(
        &self,
        haystack: &'a str,
        config: SearchConfig,
        limit: usize,
    ) -> Result<Vec<StructuredMatch<'a>>, SearchError> {
        self.search_keeping::<String>(haystack, config, limit)
    }

    /// Like [`Self::search`], but fast paths may leave `path` empty to skip allocating it.
    pub(crate) fn search_spans<'a>(
        &self,
        haystack: &'a str,
        config: SearchConfig,
        limit: usize,
    ) -> Result<Vec<StructuredMatch<'a>>, SearchError> {
        self.search_keeping::<()>(haystack, config, limit)
    }

    /// Searches past a leading byte order mark, which the YAML parser excludes from its
    /// offsets, then shifts match offsets back to the original haystack.
    fn search_keeping<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        config: SearchConfig,
        limit: usize,
    ) -> Result<Vec<StructuredMatch<'a>>, SearchError> {
        let Some(document) = haystack.strip_prefix(BYTE_ORDER_MARK) else {
            return self.search_document::<P>(haystack, config, limit);
        };
        let mut matches = self.search_document::<P>(document, config, limit)?;
        shift_matches(&mut matches, BYTE_ORDER_MARK.len());
        Ok(matches)
    }

    fn search_document<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        config: SearchConfig,
        limit: usize,
    ) -> Result<Vec<StructuredMatch<'a>>, SearchError> {
        if limit == 0 || haystack.is_empty() {
            return Ok(Vec::new());
        }
        // The byte scanners and parsers recurse per nesting level.
        with_stack_room(|| self.search_nonempty_document::<P>(haystack, config, limit))
    }

    fn search_nonempty_document<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        config: SearchConfig,
        limit: usize,
    ) -> Result<Vec<StructuredMatch<'a>>, SearchError> {
        let mode = config.structured_mode().unwrap_or(StructuredMode::Json);
        if mode == StructuredMode::Json
            && let Some(extent) = json_stream_extent(haystack.as_bytes())
        {
            return self.search_json_stream::<P>(haystack, extent, config, limit);
        }
        self.search_one_document::<P>(haystack, config, limit)
    }

    /// Searches `haystack` as one document of its format, or a stream of YAML documents,
    /// which the parser reads. Each value of a JSON stream is searched so. Callers pass a
    /// haystack that is not empty and a `limit` above zero, and give the search stack room.
    fn search_one_document<'a, P: HitPath>(
        &self,
        haystack: &'a str,
        config: SearchConfig,
        limit: usize,
    ) -> Result<Vec<StructuredMatch<'a>>, SearchError> {
        let mode = config.structured_mode().unwrap_or(StructuredMode::Json);
        if self.can_use_fast_predicates(config)
            && let Some(matches) = self.fast_predicate_search::<P>(haystack, mode)
        {
            return Ok(truncate_structured_matches(matches, limit));
        }

        if self.compiled_predicates.is_empty()
            && let Some(result) = self.fast_search::<P>(haystack, config, mode, limit)
        {
            return Ok(truncate_structured_matches(result?, limit));
        }

        let collector_limit = if config.structured_predicate_handling()
            == crate::StructuredPredicateHandling::Error
        {
            usize::MAX
        } else {
            limit
        };
        let mut collector = MatchCollector::new(self, haystack, config, collector_limit);

        match mode {
            StructuredMode::Json => {
                let root: json_spanned::Value =
                    json_spanned_value::from_str(haystack).map_err(|error| {
                        SearchError::StructuredParse {
                            mode: StructuredMode::Json,
                            message: error.to_string(),
                        }
                    })?;
                collector.visit_json(&root)?;
            }
            StructuredMode::Yaml => {
                collector.visit_yaml_stream(haystack)?;
            }
            StructuredMode::Toml => {
                let arena = toml_spanner::Arena::new();
                let document = toml_spanner::parse(haystack, &arena).map_err(|error| {
                    SearchError::StructuredParse {
                        mode: StructuredMode::Toml,
                        message: error.to_string(),
                    }
                })?;
                collector.visit_toml(document.table().as_item())?;
            }
        }

        Ok(collector.finish(limit))
    }
}

impl PatternMatcher {
    fn new(
        patterns: Vec<(usize, Vec<u8>)>,
        config: SearchConfig,
    ) -> Result<Option<Self>, SearchError> {
        if patterns.is_empty() {
            return Ok(None);
        }

        let pattern_lengths = patterns.iter().map(|(_, pattern)| pattern.len()).collect();
        let pattern_indexes = patterns.iter().map(|(index, _)| *index).collect::<Vec<_>>();
        let owned_patterns = patterns
            .into_iter()
            .map(|(_, pattern)| pattern)
            .collect::<Vec<_>>();
        let engine = if owned_patterns.len() == 1 {
            let normalized = normalize_pattern(owned_patterns[0].as_slice(), config.case_mode());
            ExactStructuredEngine::Single(Box::new(SinglePatternEngine::new(normalized)))
        } else {
            ExactStructuredEngine::Multi(Box::new(MultiPatternEngine::new(
                &owned_patterns,
                config,
            )?))
        };

        let prefilter_needle = match &engine {
            ExactStructuredEngine::Single(engine) => Some(Needle::new(
                engine.pattern_bytes(),
                config.case_mode() == CaseMode::AsciiInsensitive,
            )),
            ExactStructuredEngine::Multi(_) => None,
        };

        Ok(Some(Self {
            engine,
            prefilter_needle,
            pattern_lengths,
            pattern_indexes,
        }))
    }

    fn any_pattern_matches(&self, haystack: &[u8], config: SearchConfig) -> bool {
        match &self.engine {
            ExactStructuredEngine::Single(engine) => engine.contains(haystack, config),
            ExactStructuredEngine::Multi(engine) => {
                let matches = engine.search(haystack, config, &self.pattern_lengths, 1);
                !matches.is_empty()
            }
        }
    }

    /// The single pattern's whole-document finder; `None` for multi-pattern matchers.
    fn prefilter_needle(&self) -> Option<&Needle> {
        self.prefilter_needle.as_ref()
    }

    fn for_each_match(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        mut callback: impl FnMut(usize),
    ) {
        match &self.engine {
            ExactStructuredEngine::Single(engine) => {
                if engine.contains(haystack, config) {
                    callback(self.pattern_indexes[0]);
                }
            }
            ExactStructuredEngine::Multi(engine) => {
                let matches = engine.search(haystack, config, &self.pattern_lengths, usize::MAX);
                let mut matched_patterns = SmallVec::<[usize; 4]>::new();

                for matched in matches {
                    if matched_patterns.contains(&matched.pattern_index) {
                        continue;
                    }
                    matched_patterns.push(matched.pattern_index);
                }

                matched_patterns.sort_unstable();
                for pattern_index in matched_patterns {
                    callback(self.pattern_indexes[pattern_index]);
                }
            }
        }
    }
}

fn classify_structured_pattern(pattern: &[u8], pattern_index: usize) -> StructuredPatternKind {
    let Ok(text) = std::str::from_utf8(pattern) else {
        return StructuredPatternKind::Value;
    };

    if is_predicate_pattern(text) {
        return StructuredPatternKind::Predicate;
    }
    if !text.starts_with('$') {
        return StructuredPatternKind::Value;
    }
    // The parser splits at the `=` outside quoted keys, so `$["a=b"]` stays a path.
    match parse_path_pattern(pattern, pattern_index) {
        Some(query) if query.expected_value.is_some() => {
            StructuredPatternKind::PathValue(Some(query))
        }
        Some(query) => StructuredPatternKind::Path(Some(query)),
        None if pattern.contains(&b'=') => StructuredPatternKind::PathValue(None),
        None => StructuredPatternKind::Path(None),
    }
}

/// Moves the offsets of `matches`, found in a slice that starts `offset` bytes into a
/// haystack, to that haystack.
fn shift_matches(matches: &mut [StructuredMatch<'_>], offset: usize) {
    for matched in matches {
        matched.start += offset;
        matched.end += offset;
    }
}

fn truncate_structured_matches<'a>(
    mut matches: Vec<StructuredMatch<'a>>,
    limit: usize,
) -> Vec<StructuredMatch<'a>> {
    matches.truncate(limit);
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StructuredMatchKind;
    use crate::json_scanner::json_value_stream;

    #[test]
    fn shifted_matches_point_into_the_haystack_their_slice_was_cut_from() {
        let haystack = "{\"a\":1}\n{\"a\":2}";
        let second = &haystack[8..];
        let found = |start: usize, end: usize| StructuredMatch {
            start,
            end,
            pattern_index: 0,
            path: String::from("$.a"),
            node_text: &second[start..end],
            match_kind: StructuredMatchKind::Value,
        };
        let mut matches = [found(5, 6), found(0, 7)];

        shift_matches(&mut matches, 8);

        assert_eq!(
            matches.map(|matched| &haystack[matched.start..matched.end]),
            ["2", "{\"a\":2}"]
        );
    }

    #[test]
    fn named_node_texts_are_found_by_exact_text_however_many_names_share_a_length() {
        let mut few = PatternsByText::default();
        few.name(b"$.a.name".to_vec(), 0);
        few.name(b"$.a.tier".to_vec(), 1);
        few.name(b"$.a.tier".to_vec(), 2);
        let mut many = PatternsByText::default();
        for index in 0..20 {
            many.name(format!("$.k{index:02}").into_bytes(), index);
        }

        assert_eq!(
            few.get(b"$.a.tier").map(|found| found.to_vec()),
            Some(vec![1, 2])
        );
        assert_eq!(
            few.get(b"$.a.name").map(|found| found.to_vec()),
            Some(vec![0])
        );
        assert!(few.get(b"$.a.nome").is_none());
        assert!(few.get(b"$.a").is_none());
        assert!(few.names_length(8) && !few.names_length(3) && !few.names_length(100));
        assert_eq!(
            many.get(b"$.k13").map(|found| found.to_vec()),
            Some(vec![13])
        );
        assert!(many.get(b"$.k99").is_none());
    }

    #[test]
    fn unlimited_json_stream_searches_match_limited_ones_read_in_order() {
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .build();
        let line = |index: usize| {
            format!(
                "{{\"id\":{index},\"latency_ms\":{},\"level\":\"{}\"}}\n",
                (index * 37) % 2000,
                ["INFO", "WARN", "ERROR"][index % 3]
            )
        };
        let lines = |count: usize| (0..count).map(line).collect::<String>();
        // An array of numbers, which no search splits, in a value long enough to be outsized
        // among eight threads' shares of the streams below.
        let long = format!(
            "{{\"latency_ms\":1999,\"level\":\"WARN\",\"samples\":[{}1950]}}\n",
            "1,".repeat(74_000)
        );
        let pretty = "{\n  \"latency_ms\": 1998,\n  \"level\": \"WARN\"\n}\n";
        let streams = [
            // Tasks that start at lines find every value.
            lines(6_000),
            // A task that starts inside the value spread over lines goes astray.
            format!("{}{pretty}{}", lines(3_000), lines(3_000)),
            // A value that the parser rejects, with matches after it.
            format!("{}{{\"latency_ms\":1997,}}\n{}", lines(3_000), lines(3_000)),
            // Outsized values: first, next to each other, and apart.
            format!("{long}{}", lines(2_000)),
            format!("{}{long}{long}{}", lines(500), lines(500)),
            format!("{long}{}{long}{}{long}{}", line(1), line(2), line(3)),
        ];
        let pattern_sets: [&[&str]; 3] = [
            &["$.latency_ms > 1900"],
            &["WARN", "1950"],
            &["$.level", "$.latency_ms > 1990"],
        ];
        let pools = [2, 8].map(|threads| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("a small pool starts")
        });

        let mut searches = Vec::new();
        for patterns in pattern_sets {
            let patterns: Vec<Vec<u8>> = patterns
                .iter()
                .map(|pattern| pattern.as_bytes().to_vec())
                .collect();
            let engine = StructuredEngine::new(&patterns, config).expect("patterns should compile");
            for stream in &streams {
                for pool in &pools {
                    let in_tasks = pool.install(|| engine.search(stream, config, usize::MAX));
                    // Any limit has the values read and searched one after another.
                    let in_order = pool.install(|| engine.search(stream, config, usize::MAX - 1));
                    searches.push((in_tasks, in_order));
                }
            }
        }

        for (search, (in_tasks, in_order)) in searches.iter().enumerate() {
            assert_eq!(in_tasks, in_order, "search {search}");
        }
        let in_order = searches.iter().map(|(_, in_order)| in_order);
        let found_fewest = in_order.clone().flatten().map(Vec::len).min();
        // Two of the pattern sets have the parser read the values, in either pool.
        assert_eq!(in_order.filter(|searched| searched.is_err()).count(), 4);
        assert_eq!(found_fewest, Some(3));
    }

    #[test]
    fn json_stream_predicates_match_searching_each_value_alone() {
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .build();
        let mut stream = (0..600)
            .map(|index| {
                format!(
                    "{{\"id\":{index},\"latency_ms\":{},\"level\":\"{}\",\
                     \"tags\":[{{\"n\":{}}}]}}\n",
                    (index * 37) % 2000,
                    ["INFO", "WARN", "ERROR"][index % 3],
                    index % 5
                )
            })
            .collect::<String>();
        // One value large enough to be split across threads.
        let items = (0..9_000)
            .map(|index| format!("{{\"latency_ms\":{},\"level\":\"ERROR\"}}", index % 2000))
            .collect::<Vec<_>>();
        stream.push_str(&format!("{{\"items\":[{}]}}\n", items.join(",")));
        // Several such values in a row, which take a task each, before one that outweighs
        // those after it.
        let batch = format!("{{\"items\":[{}]}}\n", items[..4_500].join(","));
        stream.push_str(&batch.repeat(3));
        stream.push_str(&format!(
            "{{\"items\":[{}]}}\n",
            vec![items.join(","); 8].join(",")
        ));
        stream.push_str(&batch);
        // Values spread over lines, so tasks that start at a line start inside one go astray.
        for index in 0..900 {
            stream.push_str(&format!(
                "{{\n  \"latency_ms\": {},\n  \"level\": \"WARN\",\n  \
                 \"note\": \"caf\u{e9} {index}\"\n}}\n",
                1000 + index
            ));
        }
        // Short lines again, a long run of whitespace, and a last value cut short.
        for index in 0..2_000 {
            stream.push_str(&format!(
                "{{\"latency_ms\":{},\"level\":\"ERROR\"}}\n",
                index + 5
            ));
        }
        stream.push_str(&" \n".repeat(40_000));
        stream.push_str("{\"latency_ms\":1999,\"level\":\"ERROR\"}\n");
        // The parser, which reads values for patterns other than predicates, rejects a last
        // value cut short; the byte scanner reads what there is of it.
        let cut_short = format!("{stream}{{\"latency_ms\":1950,\"level\"");
        // Predicates fold values in place; the other patterns search each as a document.
        let pattern_sets: [&[&str]; 5] = [
            &["$.latency_ms > 1900"],
            &["$.latency_ms > 1500 AND $.level = \"ERROR\""],
            &[
                "$.latency_ms > 1900",
                "$.n = 4 AND $.n < 9",
                "$.level = \"WARN\"",
            ],
            &["WARN", "caf\u{e9} 7"],
            &["$.level", "$.latency_ms > 1990"],
        ];
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("a small pool starts");

        for patterns in pattern_sets {
            let patterns: Vec<Vec<u8>> = patterns
                .iter()
                .map(|pattern| pattern.as_bytes().to_vec())
                .collect();
            let engine = StructuredEngine::new(&patterns, config).expect("patterns should compile");
            for stream in [&stream, &cut_short] {
                let alone = json_value_stream(stream.as_bytes())
                    .expect("lines should be a stream")
                    .into_iter()
                    .map(|(start, end)| {
                        let mut found = engine.search(&stream[start..end], config, usize::MAX)?;
                        shift_matches(&mut found, start);
                        Ok(found)
                    })
                    .collect::<Result<Vec<_>, SearchError>>()
                    .map(|found| found.concat());

                let unlimited = pool.install(|| engine.search(stream, config, usize::MAX));

                assert_eq!(unlimited, alone, "{patterns:?}");
                let Ok(alone) = alone else {
                    assert!(
                        std::ptr::eq(stream, &cut_short),
                        "{patterns:?} should parse"
                    );
                    continue;
                };
                assert!(alone.len() > 30, "{patterns:?} should match often");
                for limit in [alone.len() + 1, alone.len(), 7, 1] {
                    let found = pool.install(|| engine.search(stream, config, limit));

                    let expected = &alone[..limit.min(alone.len())];
                    assert_eq!(found.as_deref(), Ok(expected), "{patterns:?} up to {limit}");
                }
            }
        }
    }

    #[test]
    fn yaml_streams_are_read_by_the_parser_whatever_the_patterns() {
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Yaml))
            .build();
        // A sequence at its key's indentation, and a scalar with an escape.
        let stream = "kind: Pod\n---\nspec:\n  containers:\n  - name: \"a\\x70p\"\n    \
                      image: nginx\n";
        let pattern_sets: [&[&str]; 2] = [&["$.containers[0].image = \"nginx\""], &["app"]];

        let found = pattern_sets.map(|patterns| {
            let patterns: Vec<Vec<u8>> = patterns
                .iter()
                .map(|pattern| pattern.as_bytes().to_vec())
                .collect();
            let engine = StructuredEngine::new(&patterns, config).expect("patterns should compile");
            let matches = engine
                .search(stream, config, usize::MAX)
                .expect("stream should parse");
            matches
                .into_iter()
                .map(|matched| matched.path)
                .collect::<Vec<_>>()
        });

        assert_eq!(
            found,
            [
                ["$[1].spec.containers[0].image"],
                ["$[1].spec.containers[0].name"]
            ]
        );
    }

    #[test]
    fn path_patterns_leave_yaml_streams_to_the_parser() {
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Yaml))
            .build();
        let stream = "spec:\n  replicas: 4\n---\nspec:\n  replicas: 5\n";
        let engine = StructuredEngine::new(&[b"$[1].spec.replicas".to_vec()], config)
            .expect("pattern should compile");

        let found = engine
            .search(stream, config, usize::MAX)
            .expect("stream should parse");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "$[1].spec.replicas");
        assert_eq!(found[0].node_text, "5");
    }

    #[test]
    fn predicate_item_needles_are_the_longest_string_one_object_must_hold() {
        let needle = |patterns: &[&str]| {
            let patterns: Vec<Vec<u8>> = patterns
                .iter()
                .map(|pattern| pattern.as_bytes().to_vec())
                .collect();
            let config = SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Yaml))
                .build();
            StructuredEngine::new(&patterns, config)
                .expect("patterns should compile")
                .predicate_item_needle
                .map(|needle| needle.as_bytes().to_vec())
        };

        let longest_string = needle(&["$.code=\"E1\" AND $.status=\"failed\" AND $.retries>3"]);
        let numbers_only = needle(&["$.retries>3 AND $.status=500"]);
        let one_byte_string = needle(&["$.retries>3 AND $.code=\"E\""]);
        let indexed_path = needle(&["$.tags[0]=\"failed\" AND $.code=\"E1\""]);
        let two_patterns = needle(&[
            "$.status=\"failed\" AND $.retries>3",
            "$.status=\"failed\" AND $.retries<1",
        ]);

        assert_eq!(longest_string, Some(b"failed".to_vec()));
        assert_eq!(numbers_only, None);
        assert_eq!(one_byte_string, None);
        assert_eq!(indexed_path, None);
        assert_eq!(two_patterns, None);
    }
}
