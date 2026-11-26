// The Python bindings' macros expand to `unsafe` blocks of their own.
#![cfg_attr(not(feature = "python"), forbid(unsafe_code))]
//! Fast multi-pattern text search: exact literals, fuzzy matches within an edit distance,
//! boolean queries, and paths, values, and predicates in JSON, YAML, and TOML documents.
//!
//! Build a [`Searcher`] from patterns and a [`SearchConfig`], then search text in memory
//! ([`Searcher::find`], [`Searcher::find_lines`], [`Searcher::count_matching_lines`]) or
//! summarize a directory tree with [`Searcher::report_path`].
//!
//! # Examples
//!
//! ```
//! use byteseek::{CaseMode, SearchConfig, Searcher};
//!
//! let config = SearchConfig::builder()
//!     .case_mode(CaseMode::AsciiInsensitive)
//!     .build();
//! let searcher = Searcher::new(["error", "timeout"], config)?;
//!
//! let text = "ok\nERROR: disk full\nrequest Timeout\n";
//! let lines = searcher.find_lines(text);
//! assert_eq!(lines.len(), 2);
//! assert_eq!(lines[0].line_number, 2);
//! assert_eq!(searcher.count_matching_lines(text), 2);
//! # Ok::<(), byteseek::SearchError>(())
//! ```
//!
//! The `byteseek` command-line tool, a drop-in replacement for ripgrep, and the [`cli`] module
//! it runs are built by the default `cli` feature; library users can turn them off with
//! `default-features = false`.

mod approx;
mod boolean_query;
mod chunks;
#[cfg(feature = "cli")]
pub mod cli;
mod config;
mod exact;
mod jq_path;
mod json_scanner;
mod match_types;
mod matcher;
mod occurrences;
mod profiles;
#[cfg(feature = "python")]
mod python_bindings;
mod reporting;
mod searcher;
mod structured;
mod structured_predicate;
mod toml_scanner;
mod watch;
mod yaml_scanner;

pub use config::{
    CaseMode, PathOrder, SearchConfig, SearchConfigBuilder, StructuredMode,
    StructuredPredicateHandling,
};
pub use match_types::{
    FileMatchResult, LineMatch, Match, OwnedStructuredMatch, SearchError,
    StructuredFileMatchResult, StructuredMatch, StructuredMatchKind,
};
pub use reporting::{
    DirectoryHotspot, FileHotspot, PatternComparisonRow, PatternFrequency, SearchReport,
    SearchReportStats, StructuredPathFrequency, StructuredSearchReport, StructuredValueFrequency,
};
pub use searcher::Searcher;

#[cfg(feature = "cli")]
pub(crate) use matcher::count_lines;
pub(crate) use matcher::{
    BYTE_ORDER_MARK, LineCounter, append_span, build_automaton, build_line_matcher,
    effective_limit, flush_multi_group, fold_ascii, fold_ascii_bytes, is_word_boundary,
    line_end_from, normalize_haystack, normalize_pattern, select_anchor, select_best_pending_match,
    short_bytes_eq, unfold_ascii, window_matches_ascii_insensitive, word_run_len,
};
pub(crate) use searcher::ScannedTextFile;
#[cfg(feature = "cli")]
pub(crate) use searcher::{LineVisit, MatchedLine};
#[cfg(feature = "cli")]
pub(crate) use watch::Watch;
pub(crate) use watch::{Watched, Watcher};
