mod lines;
mod parallel;
mod paths;
mod stream;
#[cfg(test)]
mod tests;

#[cfg(feature = "cli")]
pub(crate) use lines::{LineVisit, MatchedLine};
pub(crate) use paths::ScannedTextFile;

use crate::approx::ApproximateEngine;
use crate::boolean_query::BooleanQueryEngine;
use crate::exact::{BlockPlan, MultiPatternEngine, PairScanner, SinglePatternEngine};
use crate::structured::StructuredEngine;
use crate::{
    CaseMode, Match, SearchConfig, SearchError, StructuredMatch, effective_limit, normalize_pattern,
};

/// High-performance literal matcher for one or many patterns.
///
/// The matcher operates on bytes, so offsets are byte-based rather than char-indexed.
pub struct Searcher {
    config: SearchConfig,
    pub(crate) pattern_labels: Vec<Box<str>>,
    pattern_lengths: Vec<usize>,
    max_pattern_len: usize,
    has_newline_patterns: bool,
    engine: Engine,
    /// Counts matching lines of large inputs for small exact pattern sets.
    line_pairs: Option<PairScanner>,
}

impl std::fmt::Debug for Searcher {
    // Compiled engines hold large automata; the configuration and patterns identify a searcher.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Searcher")
            .field("config", &self.config)
            .field("patterns", &self.pattern_labels)
            .finish_non_exhaustive()
    }
}

enum Engine {
    Single(Box<SinglePatternEngine>),
    Multi(Box<MultiPatternEngine>),
    Approximate(ApproximateEngine),
    Boolean(Box<BooleanQueryEngine>),
    Structured(Box<StructuredEngine>),
}

impl Searcher {
    /// Compiles one or more patterns into an optimized matcher.
    ///
    /// # Arguments
    ///
    /// * `patterns` - Literal patterns to search for.
    /// * `config` - Matching behavior and result limits.
    ///
    /// # Returns
    ///
    /// A compiled matcher optimized for single-pattern or multi-pattern search.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::EmptyPatternSet`] when `patterns` is empty.
    /// Returns [`SearchError::EmptyPattern`] when any pattern is empty.
    /// Returns [`SearchError::PatternSetTooLarge`] when automaton state count exceeds `u32::MAX`.
    /// Returns [`SearchError::AutomatonBuild`] when the multi-pattern backend fails to compile.
    /// Returns [`SearchError::EditDistanceTooLarge`] when the configured edit distance exceeds
    /// `u32::MAX`.
    /// Returns [`SearchError::BooleanApproximateUnsupported`] or
    /// [`SearchError::StructuredApproximateUnsupported`] when an edit distance is combined
    /// with boolean queries or a structured mode.
    /// Returns [`SearchError::InvalidBooleanPattern`] when a boolean query does not parse, and
    /// [`SearchError::InvalidStructuredPattern`] when a structured predicate does not.
    pub fn new<I, P>(patterns: I, config: SearchConfig) -> Result<Self, SearchError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        let mut raw_patterns = Vec::new();
        let mut pattern_labels = Vec::new();

        for (index, pattern) in patterns.into_iter().enumerate() {
            let pattern = pattern.as_ref();
            if pattern.is_empty() {
                return Err(SearchError::EmptyPattern { index });
            }

            raw_patterns.push(pattern.as_bytes().to_vec());
            pattern_labels.push(pattern.to_owned().into_boxed_str());
        }

        if raw_patterns.is_empty() {
            return Err(SearchError::EmptyPatternSet);
        }

        let has_newline_patterns = raw_patterns
            .iter()
            .any(|pattern| pattern.iter().any(|&byte| byte == b'\n' || byte == b'\r'));
        let pattern_lengths = raw_patterns.iter().map(Vec::len).collect::<Vec<_>>();
        let max_pattern_len = pattern_lengths.iter().copied().max().unwrap_or(1);

        let engine = if config.structured_mode().is_some() {
            if config.max_edit_distance().is_some() {
                return Err(SearchError::StructuredApproximateUnsupported);
            }
            Engine::Structured(Box::new(StructuredEngine::new(&raw_patterns, config)?))
        } else if config.boolean_queries() {
            if config.max_edit_distance().is_some() {
                return Err(SearchError::BooleanApproximateUnsupported);
            }
            Engine::Boolean(Box::new(BooleanQueryEngine::new(&raw_patterns, config)?))
        } else if config.max_edit_distance().is_some() {
            Engine::Approximate(ApproximateEngine::new(&raw_patterns, config)?)
        } else if raw_patterns.len() == 1 {
            let normalized_single =
                normalize_pattern(raw_patterns[0].as_slice(), config.case_mode());
            Engine::Single(Box::new(SinglePatternEngine::new(normalized_single)))
        } else {
            Engine::Multi(Box::new(MultiPatternEngine::new(&raw_patterns, config)?))
        };

        // A pair scan follows no pattern across a line end.
        let pair_scannable = !has_newline_patterns;
        let line_pairs = match &engine {
            Engine::Single(single) if pair_scannable => {
                // The insensitive single-pattern count stops on one anchor byte, which is as
                // fast as a pair scan when that byte is rare.
                let rival = (config.case_mode() == CaseMode::AsciiInsensitive)
                    .then(|| single.anchor_byte());
                PairScanner::new(&raw_patterns, config, rival)
            }
            Engine::Multi(_) if pair_scannable => PairScanner::new(&raw_patterns, config, None),
            Engine::Single(_)
            | Engine::Multi(_)
            | Engine::Approximate(_)
            | Engine::Boolean(_)
            | Engine::Structured(_) => None,
        };

        Ok(Self {
            config,
            pattern_labels,
            pattern_lengths,
            max_pattern_len,
            has_newline_patterns,
            engine,
            line_pairs,
        })
    }

    /// Returns the search configuration used by this matcher.
    ///
    /// # Returns
    ///
    /// The configuration the searcher was built with.
    #[must_use]
    pub fn config(&self) -> SearchConfig {
        self.config
    }

    /// Returns the number of compiled patterns.
    ///
    /// # Returns
    ///
    /// How many patterns were supplied to [`Searcher::new`].
    #[must_use]
    pub fn pattern_count(&self) -> usize {
        self.pattern_lengths.len()
    }

    /// Finds all matches in `haystack` according to the configured semantics.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text to scan.
    ///
    /// # Returns
    ///
    /// Matches sorted by discovery order. Offsets are byte-based.
    #[inline]
    #[must_use]
    pub fn find(&self, haystack: &str) -> Vec<Match> {
        self.try_find(haystack).unwrap_or_default()
    }

    /// Finds all matches in `haystack`, returning structured parse errors when relevant.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text to scan.
    ///
    /// # Returns
    ///
    /// Matches sorted by discovery order. Offsets are byte-based.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::StructuredParse`] when structured mode is enabled and the input
    /// cannot be parsed as the configured AST format.
    pub fn try_find(&self, haystack: &str) -> Result<Vec<Match>, SearchError> {
        self.try_find_with_limit(haystack, None)
    }

    /// Finds the first match in `haystack`, if one exists.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text to scan.
    ///
    /// # Returns
    ///
    /// The earliest accepted match or `None`.
    #[inline]
    #[must_use]
    pub fn find_first(&self, haystack: &str) -> Option<Match> {
        self.try_find_first(haystack).unwrap_or(None)
    }

    /// Finds the first match in `haystack`, returning structured parse errors when relevant.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text to scan.
    ///
    /// # Returns
    ///
    /// The earliest accepted match or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::StructuredParse`] when structured mode is enabled and the input
    /// cannot be parsed as the configured AST format.
    #[inline]
    pub fn try_find_first(&self, haystack: &str) -> Result<Option<Match>, SearchError> {
        if self.config.max_matches() == Some(0) {
            return Ok(None);
        }
        match &self.engine {
            Engine::Structured(_) => self
                .try_find_with_limit(haystack, Some(1))
                .map(|matches| matches.into_iter().next()),
            Engine::Single(_) | Engine::Multi(_) | Engine::Approximate(_) | Engine::Boolean(_) => {
                Ok(self.find_first_in_bytes(haystack.as_bytes()))
            }
        }
    }

    /// Finds the first match in bytes that need not be UTF-8.
    ///
    /// Callers handle a match limit of zero and route structured engines elsewhere.
    #[inline]
    fn find_first_in_bytes(&self, haystack: &[u8]) -> Option<Match> {
        match &self.engine {
            Engine::Single(engine) => engine.find_first(haystack, self.config),
            Engine::Multi(engine) => {
                engine.find_first(haystack, self.config, &self.pattern_lengths)
            }
            Engine::Approximate(engine) => engine.find_first(haystack, self.config),
            Engine::Boolean(engine) => engine.find_first(haystack, self.config),
            Engine::Structured(_) => {
                // `debug_assert!` is checked in debug builds only.
                debug_assert!(false, "structured searches dispatch on text");
                None
            }
        }
    }

    /// Whether `haystack` holds a match, under the conditions of [`Self::find_first_in_bytes`].
    #[inline]
    fn contains_in_bytes(&self, haystack: &[u8]) -> bool {
        match &self.engine {
            Engine::Multi(engine) => engine.contains(haystack, self.config, &self.pattern_lengths),
            Engine::Single(_)
            | Engine::Approximate(_)
            | Engine::Boolean(_)
            | Engine::Structured(_) => self.find_first_in_bytes(haystack).is_some(),
        }
    }

    /// Returns true if at least one match exists in `haystack`.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text to scan.
    ///
    /// # Returns
    ///
    /// Whether any accepted match exists.
    #[inline]
    #[must_use]
    pub fn contains(&self, haystack: &str) -> bool {
        self.try_contains(haystack).unwrap_or(false)
    }

    /// Returns true if at least one match exists in `haystack`.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text to scan.
    ///
    /// # Returns
    ///
    /// Whether any accepted match exists.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::StructuredParse`] when structured mode is enabled and the input
    /// cannot be parsed as the configured AST format.
    #[inline]
    pub fn try_contains(&self, haystack: &str) -> Result<bool, SearchError> {
        if self.config.max_matches() == Some(0) {
            return Ok(false);
        }
        match &self.engine {
            Engine::Structured(_) => self.try_find_first(haystack).map(|found| found.is_some()),
            Engine::Single(_) | Engine::Multi(_) | Engine::Approximate(_) | Engine::Boolean(_) => {
                Ok(self.contains_in_bytes(haystack.as_bytes()))
            }
        }
    }

    /// Counts matches in `haystack` while honoring configured limits.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text to scan.
    ///
    /// # Returns
    ///
    /// Number of emitted matches.
    #[must_use]
    pub fn count(&self, haystack: &str) -> usize {
        self.try_count(haystack).unwrap_or_default()
    }

    /// Counts matches while preserving structured parsing errors.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text to scan.
    ///
    /// # Returns
    ///
    /// Number of emitted matches.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::StructuredParse`] when structured mode is enabled and the input
    /// cannot be parsed as the configured format. Predicate errors are also propagated when strict
    /// structured predicate handling is enabled.
    pub fn try_count(&self, haystack: &str) -> Result<usize, SearchError> {
        let limit = effective_limit(self.config.max_matches(), None);
        if let Some(count) = self.count_exact(haystack.as_bytes(), limit) {
            return Ok(count);
        }

        self.try_find_with_limit(haystack, None)
            .map(|matches| matches.len())
    }

    /// Finds structured matches with jq-like path metadata.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Structured input text to parse and search.
    ///
    /// # Returns
    ///
    /// Matches with their jq-like paths, in document order.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::StructuredModeDisabled`] if the searcher was built without a
    /// structured mode. Returns [`SearchError::StructuredParse`] when parsing fails.
    pub fn find_structured<'a>(
        &self,
        haystack: &'a str,
    ) -> Result<Vec<StructuredMatch<'a>>, SearchError> {
        match &self.engine {
            Engine::Structured(engine) => engine.search(
                haystack,
                self.config,
                effective_limit(self.config.max_matches(), None),
            ),
            Engine::Single(_) | Engine::Multi(_) | Engine::Approximate(_) | Engine::Boolean(_) => {
                Err(SearchError::StructuredModeDisabled)
            }
        }
    }

    #[inline]
    fn find_with_limit(&self, haystack: &str, limit_override: Option<usize>) -> Vec<Match> {
        self.try_find_with_limit(haystack, limit_override)
            .unwrap_or_default()
    }

    /// Dispatches on `&str` so structured engines never revalidate UTF-8.
    #[inline]
    fn try_find_with_limit(
        &self,
        haystack: &str,
        limit_override: Option<usize>,
    ) -> Result<Vec<Match>, SearchError> {
        if let Engine::Structured(engine) = &self.engine {
            let limit = effective_limit(self.config.max_matches(), limit_override);
            return structured_spans(engine, haystack, self.config, limit);
        }
        Ok(self.find_bytes_with_limit(haystack.as_bytes(), limit_override))
    }

    fn find_bytes_with_limit(&self, haystack: &[u8], limit_override: Option<usize>) -> Vec<Match> {
        let limit = effective_limit(self.config.max_matches(), limit_override);

        // Approximate engines split their own work, since each piece needs context from the
        // text before it.
        if !self.should_parallelize(haystack.len())
            || self.config.whole_word()
            || matches!(self.engine, Engine::Approximate(_))
        {
            return self.find_sequential(haystack, limit);
        }

        // Plan once from the whole input so short chunks scan like the input they came from.
        let planned = self.block_plan(haystack);
        let search = |chunk: &[u8]| self.find_planned(chunk, limit, planned);
        if self.config.overlapping() {
            return self.find_parallel_chunks(haystack, limit, search);
        }
        match self.exact_split_points(haystack) {
            Some(splits) => parallel::search_disjoint_chunks(haystack, &splits, limit, search),
            None => self.find_sequential(haystack, limit),
        }
    }

    /// The exact engine's block scan plan for `haystack`, if it has one.
    #[inline]
    fn block_plan(&self, haystack: &[u8]) -> BlockPlan<'_> {
        match &self.engine {
            Engine::Single(engine) => engine.block_plan(haystack, self.config),
            Engine::Multi(engine) => engine.block_plan(haystack, self.config),
            Engine::Approximate(_) | Engine::Boolean(_) | Engine::Structured(_) => None,
        }
    }

    /// Searches a piece of an input with the plan [`Self::block_plan`] made for all of it.
    #[inline]
    fn find_planned(&self, piece: &[u8], limit: usize, planned: BlockPlan<'_>) -> Vec<Match> {
        match &self.engine {
            Engine::Single(engine) => engine.search_planned(piece, self.config, limit, planned),
            Engine::Multi(engine) if !self.config.overlapping() && !self.config.whole_word() => {
                engine.search_non_overlapping(piece, &self.pattern_lengths, limit, planned)
            }
            Engine::Multi(_)
            | Engine::Approximate(_)
            | Engine::Boolean(_)
            | Engine::Structured(_) => self.find_sequential(piece, limit),
        }
    }

    #[inline]
    fn find_sequential(&self, haystack: &[u8], limit: usize) -> Vec<Match> {
        match &self.engine {
            Engine::Single(engine) => engine.search(haystack, self.config, limit),
            Engine::Multi(engine) => {
                engine.search(haystack, self.config, &self.pattern_lengths, limit)
            }
            Engine::Approximate(engine) => engine.search(haystack, self.config, limit),
            Engine::Boolean(engine) => engine.search(haystack, self.config, limit),
            // Structured searches dispatch on `&str` earlier; this arm only guards that.
            Engine::Structured(engine) => std::str::from_utf8(haystack)
                .ok()
                .and_then(|text| structured_spans(engine, text, self.config, limit).ok())
                .unwrap_or_default(),
        }
    }
}

/// Runs a structured search and keeps only match spans.
fn structured_spans(
    engine: &StructuredEngine,
    haystack: &str,
    config: SearchConfig,
    limit: usize,
) -> Result<Vec<Match>, SearchError> {
    engine.search_spans(haystack, config, limit).map(|matches| {
        matches
            .into_iter()
            .map(|matched| Match {
                start: matched.start,
                end: matched.end,
                pattern_index: matched.pattern_index,
            })
            .collect()
    })
}
