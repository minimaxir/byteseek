use std::path::PathBuf;

use smallvec::SmallVec;

/// A single match result expressed as byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Match {
    /// Inclusive start byte offset in the searched string.
    pub start: usize,
    /// Exclusive end byte offset in the searched string.
    pub end: usize,
    /// Index of the pattern that matched.
    pub pattern_index: usize,
}

/// Match results for one line returned by line-oriented search methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineMatch<'a> {
    /// One-based line number in the input text.
    pub line_number: usize,
    /// Original line content (without trailing newline).
    pub line: &'a str,
    /// Match offsets relative to `line`, stored inline for the common one or two matches.
    pub matches: SmallVec<[Match; 2]>,
}

/// Indicates which structured record matched the configured pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructuredMatchKind {
    /// The jq-like path for a parsed node matched.
    Path,
    /// The scalar value for a parsed node matched.
    Value,
    /// The combined `path=value` representation matched.
    PathValue,
    /// A structured predicate matched a node within one object context.
    Predicate,
}

/// Rich match metadata for structured searches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredMatch<'a> {
    /// Inclusive start byte offset in the original haystack.
    pub start: usize,
    /// Exclusive end byte offset in the original haystack.
    pub end: usize,
    /// Index of the pattern that matched.
    pub pattern_index: usize,
    /// jq-like path identifying the matched node.
    pub path: String,
    /// Original text for the matched node slice.
    pub node_text: &'a str,
    /// Indicates whether the path, value, or combined representation matched.
    pub match_kind: StructuredMatchKind,
}

/// Structured match metadata with owned text for file-based search results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedStructuredMatch {
    /// Inclusive start byte offset in the original file.
    pub start: usize,
    /// Exclusive end byte offset in the original file.
    pub end: usize,
    /// Index of the pattern that matched.
    pub pattern_index: usize,
    /// jq-like path identifying the matched node.
    pub path: String,
    /// Original text for the matched node slice.
    pub node_text: String,
    /// Indicates whether the path, value, or combined representation matched.
    pub match_kind: StructuredMatchKind,
}

impl From<StructuredMatch<'_>> for OwnedStructuredMatch {
    fn from(value: StructuredMatch<'_>) -> Self {
        Self {
            start: value.start,
            end: value.end,
            pattern_index: value.pattern_index,
            path: value.path,
            node_text: value.node_text.to_owned(),
            match_kind: value.match_kind,
        }
    }
}

/// Match results for a single file discovered during recursive path search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatchResult {
    /// Path to the matched file.
    pub path: PathBuf,
    /// Matches produced for the file content.
    pub matches: Vec<Match>,
}

/// Structured match results for a single file discovered during recursive path search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredFileMatchResult {
    /// Path to the matched file.
    pub path: PathBuf,
    /// Structured matches produced for the file content.
    pub matches: Vec<OwnedStructuredMatch>,
}

/// Errors produced while constructing a [`crate::Searcher`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// Pattern list cannot be empty.
    #[error("at least one pattern is required")]
    EmptyPatternSet,
    /// Empty patterns are not valid literal search terms.
    #[error("pattern at index {index} is empty")]
    EmptyPattern {
        /// Position of the invalid pattern in the original pattern list.
        index: usize,
    },
    /// Internal state count exceeded the supported automaton limit.
    #[error("pattern set is too large to compile")]
    PatternSetTooLarge,
    /// Building the multi-pattern automaton failed.
    #[error("failed to compile pattern automaton: {message}")]
    AutomatonBuild {
        /// Error detail returned by the underlying automaton builder.
        message: String,
    },
    /// The configured edit distance exceeds the supported range.
    #[error("max edit distance {distance} exceeds the supported range")]
    EditDistanceTooLarge {
        /// User-supplied edit distance that could not be represented by the backend.
        distance: usize,
    },
    /// Approximate matching is not available for structured parsing mode.
    #[error("approximate matching is not supported when structured mode is enabled")]
    StructuredApproximateUnsupported,
    /// Structured search APIs were used without configuring a structured mode.
    #[error("structured mode is not enabled for this searcher")]
    StructuredModeDisabled,
    /// Parsing the structured haystack failed.
    #[error("failed to parse structured input as {mode:?}: {message}")]
    StructuredParse {
        /// Structured parsing mode requested for the haystack.
        mode: crate::StructuredMode,
        /// Error detail from the structured parser.
        message: String,
    },
    /// A structured pattern could not be parsed or is semantically invalid.
    #[error("invalid structured pattern at index {index}: {message}")]
    InvalidStructuredPattern {
        /// Position of the invalid pattern in the original pattern list.
        index: usize,
        /// Validation detail for the rejected pattern.
        message: String,
    },
    /// Structured predicate evaluation encountered null or invalid typed data.
    #[error("structured predicate at pattern index {pattern_index} failed at {path}: {message}")]
    StructuredPredicate {
        /// Position of the predicate pattern in the original pattern list.
        pattern_index: usize,
        /// Path whose value triggered the predicate failure.
        path: String,
        /// Evaluation detail explaining the failure.
        message: String,
    },
    /// A boolean text query could not be parsed or is semantically invalid.
    #[error("invalid boolean query at index {index}: {message}")]
    InvalidBooleanPattern {
        /// Position of the invalid pattern in the original pattern list.
        index: usize,
        /// Validation detail for the rejected boolean query.
        message: String,
    },
    /// Approximate matching is not available for boolean-query mode.
    #[error("approximate matching is not supported when boolean query mode is enabled")]
    BooleanApproximateUnsupported,
    /// Walking a directory tree failed during file search.
    #[error("failed to traverse files under {path}: {message}")]
    FileTraversal {
        /// Directory or file path associated with the traversal failure.
        path: String,
        /// Error detail returned by the walker.
        message: String,
    },
    /// A file could not be opened or read during file search, or the file named as the
    /// search's path is not valid UTF-8.
    #[error("failed to read {path}: {message}")]
    FileRead {
        /// Path of the file.
        path: String,
        /// Error detail from the filesystem or from the UTF-8 check.
        message: String,
    },
}
