use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod aggregation;
mod render;

use aggregation::DisplayRoot;

#[cfg(feature = "cli")]
pub(crate) use render::{append_byte_size, decimal_len};
use render::{
    format_summary, render_counted_table, render_directory_density, render_file_table,
    render_pattern_comparison_table, render_pattern_cooccurrence_matrix, render_pattern_table,
};

/// Aggregate scan statistics produced while building a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchReportStats {
    /// Number of files scanned. Non-UTF-8 files found in a directory are skipped and not counted.
    pub scanned_files: usize,
    /// Total bytes of the scanned files.
    pub scanned_bytes: u64,
    /// Number of files with at least one accepted match.
    pub matched_files: usize,
    /// Total number of emitted matches across all matched files.
    pub total_matches: usize,
    /// Total number of lines that contain at least one accepted match.
    pub total_matching_lines: usize,
}

/// Per-pattern frequency information within a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternFrequency {
    /// Index of the original compiled pattern.
    pub pattern_index: usize,
    /// Original pattern text.
    pub pattern: String,
    /// Total number of emitted matches for this pattern.
    pub match_count: usize,
    /// Number of files where this pattern matched at least once.
    pub matched_files: usize,
}

/// Per-file hotspot information ordered by impact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHotspot {
    /// Path to the matched file.
    pub path: PathBuf,
    /// Number of emitted matches in the file.
    pub match_count: usize,
    /// Number of lines in the file that contain at least one accepted match.
    pub matching_lines: usize,
    /// Byte offset of the earliest emitted match in the file.
    pub first_match_byte: usize,
}

/// Per-directory hotspot information ordered by impact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryHotspot {
    /// Directory containing matched files. Root-level files use `"."`.
    pub path: PathBuf,
    /// Number of matched files under this directory.
    pub matched_files: usize,
    /// Total number of emitted matches under this directory.
    pub match_count: usize,
}

/// Per-file counts for each compiled pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternComparisonRow {
    /// Path to the matched file.
    pub path: PathBuf,
    /// Total number of emitted matches in the file.
    pub total_matches: usize,
    /// Number of lines in the file that contain at least one accepted match.
    pub matching_lines: usize,
    /// Per-pattern match counts in original pattern order.
    ///
    /// Use [`PatternFrequency::pattern_index`] to map a pattern back to this vector.
    pub pattern_counts: Vec<usize>,
}

/// Frequency information for a structured path among matched nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredPathFrequency {
    /// jq-like path of the matched node.
    pub path: String,
    /// Total number of emitted matches for this path.
    pub match_count: usize,
    /// Number of files where this path matched at least once.
    pub matched_files: usize,
}

/// Frequency information for node values under one structured path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredValueFrequency {
    /// Original node text for the value.
    pub value: String,
    /// Total number of emitted matches for this value.
    pub match_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReportCore {
    root: PathBuf,
    root_is_dir: bool,
    stats: SearchReportStats,
    pattern_frequencies: Vec<PatternFrequency>,
    file_hotspots: Vec<FileHotspot>,
    directory_hotspots: Vec<DirectoryHotspot>,
    pattern_comparison_rows: Vec<PatternComparisonRow>,
}

/// Report for exact or approximate path-based searches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReport {
    core: ReportCore,
}

/// Report for structured path-based searches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredSearchReport {
    core: ReportCore,
    path_frequencies: Vec<StructuredPathFrequency>,
    value_frequencies: BTreeMap<String, Vec<StructuredValueFrequency>>,
}

macro_rules! impl_common_report_api {
    ($report:ty, $summary_title:literal) => {
        impl $report {
            /// Returns the root path that was scanned.
            ///
            /// # Returns
            ///
            /// The file or directory passed when the report was built.
            #[must_use]
            pub fn root(&self) -> &Path {
                self.core.root.as_path()
            }

            fn display_root(&self) -> DisplayRoot<'_> {
                DisplayRoot::with_kind(self.root(), self.core.root_is_dir)
            }

            /// Returns aggregate scan statistics.
            ///
            /// # Returns
            ///
            /// Totals for scanned files, bytes, matches, and matching lines.
            #[must_use]
            pub fn stats(&self) -> SearchReportStats {
                self.core.stats
            }

            /// Returns pattern frequencies sorted by descending match count.
            ///
            /// # Returns
            ///
            /// One entry per pattern, most frequent first.
            #[must_use]
            pub fn pattern_frequencies(&self) -> &[PatternFrequency] {
                self.core.pattern_frequencies.as_slice()
            }

            /// Returns matched files sorted by descending impact.
            ///
            /// # Returns
            ///
            /// One entry per matched file, most matches first.
            #[must_use]
            pub fn file_hotspots(&self) -> &[FileHotspot] {
                self.core.file_hotspots.as_slice()
            }

            /// Returns matched directories sorted by descending impact.
            ///
            /// # Returns
            ///
            /// One entry per directory containing matches, most matches first.
            #[must_use]
            pub fn directory_hotspots(&self) -> &[DirectoryHotspot] {
                self.core.directory_hotspots.as_slice()
            }

            /// Returns per-file pattern counts sorted by descending total impact.
            ///
            /// # Returns
            ///
            /// One row per matched file with per-pattern counts.
            #[must_use]
            pub fn pattern_comparison_rows(&self) -> &[PatternComparisonRow] {
                self.core.pattern_comparison_rows.as_slice()
            }

            /// Renders a compact summary suitable for terminal output.
            ///
            /// # Returns
            ///
            /// A multi-line summary of scan and match totals.
            #[must_use]
            pub fn render_summary(&self) -> String {
                format_summary($summary_title, self.stats())
            }

            /// Renders the highest-impact files as an ASCII table.
            ///
            /// # Arguments
            ///
            /// * `limit` - Maximum number of file rows to render.
            ///
            /// # Returns
            ///
            /// The rendered table, or a "No data" table when nothing matched.
            #[must_use]
            pub fn render_top_files_table(&self, limit: usize) -> String {
                render_file_table(self.display_root(), self.file_hotspots(), limit)
            }

            /// Renders matched directories with proportional ASCII density bars.
            ///
            /// # Arguments
            ///
            /// * `limit` - Maximum number of directory rows to render.
            ///
            /// # Returns
            ///
            /// The rendered density chart, or a placeholder line when nothing matched.
            #[must_use]
            pub fn render_directory_density(&self, limit: usize) -> String {
                render_directory_density(self.directory_hotspots(), limit)
            }

            /// Renders per-pattern frequencies as an ASCII table.
            ///
            /// # Returns
            ///
            /// The rendered table with one row per pattern.
            #[must_use]
            pub fn render_pattern_frequency_table(&self) -> String {
                render_pattern_table(self.pattern_frequencies())
            }

            /// Renders a per-file comparison table for the most frequent patterns.
            ///
            /// # Arguments
            ///
            /// * `file_limit` - Maximum number of matched files to render.
            /// * `pattern_limit` - Maximum number of pattern columns to render.
            ///
            /// # Returns
            ///
            /// The rendered table, or a placeholder line when nothing matched.
            #[must_use]
            pub fn render_pattern_comparison_table(
                &self,
                file_limit: usize,
                pattern_limit: usize,
            ) -> String {
                render_pattern_comparison_table(
                    self.display_root(),
                    self.pattern_comparison_rows(),
                    self.pattern_frequencies(),
                    file_limit,
                    pattern_limit,
                )
            }

            /// Renders a file-level pattern co-occurrence matrix.
            ///
            /// Each cell shows `shared_files/row_files` for the row pattern.
            ///
            /// # Arguments
            ///
            /// * `pattern_limit` - Maximum number of patterns to include from the frequency
            ///   ranking.
            ///
            /// # Returns
            ///
            /// The rendered matrix, or a placeholder line when nothing matched.
            #[must_use]
            pub fn render_pattern_cooccurrence_matrix(&self, pattern_limit: usize) -> String {
                render_pattern_cooccurrence_matrix(
                    self.pattern_comparison_rows(),
                    self.pattern_frequencies(),
                    pattern_limit,
                )
            }
        }
    };
}

impl_common_report_api!(SearchReport, "Search Summary");
impl_common_report_api!(StructuredSearchReport, "Structured Search Summary");

impl StructuredSearchReport {
    /// Returns structured paths sorted by descending match count.
    ///
    /// # Returns
    ///
    /// One entry per structured path, most matches first.
    #[must_use]
    pub fn path_frequencies(&self) -> &[StructuredPathFrequency] {
        self.path_frequencies.as_slice()
    }

    /// Returns value frequencies for one structured path, if present.
    ///
    /// # Arguments
    ///
    /// * `path` - jq-like path whose node values should be summarized.
    ///
    /// # Returns
    ///
    /// Value counts for `path`, or `None` when the path never matched.
    #[must_use]
    pub fn value_frequencies_for_path(&self, path: &str) -> Option<&[StructuredValueFrequency]> {
        self.value_frequencies.get(path).map(Vec::as_slice)
    }

    /// Renders structured path frequencies as an ASCII table.
    ///
    /// # Arguments
    ///
    /// * `limit` - Maximum number of path rows to render.
    ///
    /// # Returns
    ///
    /// The rendered table with up to `limit` path rows.
    #[must_use]
    pub fn render_path_frequency_table(&self, limit: usize) -> String {
        render_counted_table(
            format_args!("Structured Path Frequencies"),
            &["Path", "Matches", "Files"],
            self.path_frequencies().iter().take(limit).map(|entry| {
                (
                    entry.path.as_str(),
                    [entry.match_count, entry.matched_files],
                )
            }),
        )
    }

    /// Renders value frequencies for one structured path as an ASCII table.
    ///
    /// # Arguments
    ///
    /// * `path` - jq-like path whose node values should be summarized.
    /// * `limit` - Maximum number of value rows to render.
    ///
    /// # Returns
    ///
    /// The rendered table, or `None` when `path` never matched.
    #[must_use]
    pub fn render_value_frequency_table(&self, path: &str, limit: usize) -> Option<String> {
        self.value_frequencies_for_path(path).map(|frequencies| {
            render_counted_table(
                format_args!("Field Value Summary: {path}"),
                &["Value", "Matches"],
                frequencies
                    .iter()
                    .take(limit)
                    .map(|entry| (entry.value.as_str(), [entry.match_count])),
            )
        })
    }
}
