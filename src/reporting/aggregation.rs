use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hashbrown::HashMap;

use crate::{Match, ScannedTextFile, SearchError, Searcher, StructuredMatch};

use super::render::display_label;
use super::{
    DirectoryHotspot, FileHotspot, PatternComparisonRow, PatternFrequency, ReportCore,
    SearchReport, SearchReportStats, StructuredPathFrequency, StructuredSearchReport,
    StructuredValueFrequency,
};

// Sorts by the fields before `;` descending, then by the tie-breakers after it ascending.
macro_rules! sort_frequencies {
    ($rows:expr, $first:ident $(, $descending:ident)*; $($ascending:ident),+) => {
        $rows.sort_unstable_by(|left, right| {
            right.$first.cmp(&left.$first)
                $(.then_with(|| right.$descending.cmp(&left.$descending)))*
                $(.then_with(|| left.$ascending.cmp(&right.$ascending)))+
        });
    };
}

struct MatchedFile<Extra> {
    match_count: usize,
    matching_lines: usize,
    first_match_byte: usize,
    pattern_counts: Vec<usize>,
    extra: Extra,
}

impl<Extra> MatchedFile<Extra> {
    fn from_matches<M: ReportMatch>(
        bytes: &[u8],
        pattern_count: usize,
        matches: &[M],
        extra: impl FnOnce() -> Extra,
    ) -> Option<Self> {
        let first_match = matches.first()?;
        let mut pattern_counts = vec![0_usize; pattern_count];
        for matched in matches {
            pattern_counts[matched.pattern_index()] += 1;
        }

        Some(Self {
            match_count: matches.len(),
            matching_lines: count_matching_lines(bytes, matches),
            first_match_byte: first_match.start(),
            pattern_counts,
            extra: extra(),
        })
    }
}

trait ReportMatch {
    fn start(&self) -> usize;
    fn pattern_index(&self) -> usize;
}

macro_rules! impl_report_match {
    ($match_type:ty) => {
        impl ReportMatch for $match_type {
            fn start(&self) -> usize {
                self.start
            }

            fn pattern_index(&self) -> usize {
                self.pattern_index
            }
        }
    };
}

impl_report_match!(Match);
impl_report_match!(StructuredMatch<'_>);

type StructuredValueCounts = HashMap<String, HashMap<String, usize>>;
type StructuredValueFrequencies = BTreeMap<String, Vec<StructuredValueFrequency>>;

fn count_structured_values(matches: &[StructuredMatch<'_>]) -> StructuredValueCounts {
    let mut counts = StructuredValueCounts::new();
    for matched in matches {
        // Borrowed-key entries allocate strings only for previously unseen paths and values.
        *counts
            .entry_ref(matched.path.as_str())
            .or_default()
            .entry_ref(matched.node_text)
            .or_insert(0) += 1;
    }
    counts
}

#[derive(Debug, Default)]
struct StructuredPathCounts {
    matched_files: usize,
    values: HashMap<String, usize>,
}

#[derive(Debug, Default)]
struct StructuredAccumulator {
    paths: HashMap<String, StructuredPathCounts>,
}

impl StructuredAccumulator {
    fn record(&mut self, matched: StructuredValueCounts) {
        for (path, values) in matched {
            let entry = self.paths.entry(path).or_default();
            entry.matched_files += 1;
            for (value, count) in values {
                *entry.values.entry(value).or_insert(0) += count;
            }
        }
    }

    fn finish(self) -> (Vec<StructuredPathFrequency>, StructuredValueFrequencies) {
        let mut path_frequencies = Vec::with_capacity(self.paths.len());
        let mut value_frequencies = BTreeMap::new();
        for (path, counts) in self.paths {
            path_frequencies.push(StructuredPathFrequency {
                path: path.clone(),
                match_count: counts.values.values().sum(),
                matched_files: counts.matched_files,
            });
            let mut entries = counts
                .values
                .into_iter()
                .map(|(value, match_count)| StructuredValueFrequency { value, match_count })
                .collect::<Vec<_>>();
            sort_frequencies!(entries, match_count; value);
            value_frequencies.insert(path, entries);
        }
        sort_frequencies!(path_frequencies, match_count, matched_files; path);
        (path_frequencies, value_frequencies)
    }
}

impl Searcher {
    /// Builds a reusable report for exact or approximate search results under `path`.
    ///
    /// Non-UTF-8 files in a directory are skipped, as in [`Searcher::find_path`], and not counted.
    ///
    /// # Arguments
    ///
    /// * `path` - File or directory to scan.
    ///
    /// # Returns
    ///
    /// A report aggregating matches across every scanned file.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::FileTraversal`] when the path cannot be walked.
    /// Returns [`SearchError::FileRead`] when a file cannot be read, or when `path` itself
    /// names a file that is not UTF-8 text.
    /// Returns [`SearchError::StructuredParse`] when the searcher is in structured mode and a
    /// candidate file cannot be parsed.
    pub fn report_path<P: AsRef<Path>>(&self, path: P) -> Result<SearchReport, SearchError> {
        let root = path.as_ref().to_path_buf();
        let scanned = self.scan_text_files(path.as_ref(), |searcher, _, contents| {
            let matches = searcher.try_find(contents)?;
            Ok(MatchedFile::from_matches(
                contents.as_bytes(),
                searcher.pattern_count(),
                matches.as_slice(),
                || (),
            ))
        })?;

        Ok(SearchReport {
            core: build_report_core(root, self, scanned, drop),
        })
    }

    /// Builds a reusable report for structured search results under `path`.
    ///
    /// Non-UTF-8 files in a directory are skipped, as in [`Searcher::find_structured_path`], and
    /// not counted.
    ///
    /// # Arguments
    ///
    /// * `path` - File or directory to scan.
    ///
    /// # Returns
    ///
    /// A report aggregating structured matches, paths, and values across files.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::StructuredModeDisabled`] when the searcher was not configured with
    /// a structured mode.
    /// Returns [`SearchError::FileTraversal`] when the path cannot be walked.
    /// Returns [`SearchError::FileRead`] when a file cannot be read, or when `path` itself
    /// names a file that is not UTF-8 text.
    /// Returns [`SearchError::StructuredParse`] when a candidate file cannot be parsed.
    pub fn structured_report_path<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<StructuredSearchReport, SearchError> {
        let root = path.as_ref().to_path_buf();
        let scanned = self.scan_text_files(path.as_ref(), |searcher, _, contents| {
            let matches = searcher.find_structured(contents)?;
            Ok(MatchedFile::from_matches(
                contents.as_bytes(),
                searcher.pattern_count(),
                matches.as_slice(),
                || count_structured_values(matches.as_slice()),
            ))
        })?;

        let mut structured = StructuredAccumulator::default();
        let core = build_report_core(root, self, scanned, |matched| structured.record(matched));
        let (path_frequencies, value_frequencies) = structured.finish();

        Ok(StructuredSearchReport {
            core,
            path_frequencies,
            value_frequencies,
        })
    }
}

fn build_report_core<Extra>(
    root: PathBuf,
    searcher: &Searcher,
    scanned: Vec<ScannedTextFile<MatchedFile<Extra>>>,
    mut record_extra: impl FnMut(Extra),
) -> ReportCore {
    let mut stats = SearchReportStats::default();
    let mut pattern_counts = vec![0_usize; searcher.pattern_count()];
    let mut pattern_files = vec![0_usize; searcher.pattern_count()];
    let mut file_hotspots = Vec::with_capacity(scanned.len());
    let mut pattern_comparison_rows = Vec::with_capacity(scanned.len());
    let mut directory_counts: HashMap<PathBuf, (usize, usize)> = HashMap::new();
    let display_root = DisplayRoot::new(root.as_path());

    for ScannedTextFile {
        path,
        byte_len,
        value,
    } in scanned
    {
        stats.scanned_files += 1;
        stats.scanned_bytes += byte_len as u64;

        let Some(matched) = value else {
            continue;
        };
        let MatchedFile {
            match_count,
            matching_lines,
            first_match_byte,
            pattern_counts: file_pattern_counts,
            extra,
        } = matched;

        stats.matched_files += 1;
        stats.total_matches += match_count;
        stats.total_matching_lines += matching_lines;

        for (pattern_index, count) in file_pattern_counts.iter().copied().enumerate() {
            if count > 0 {
                pattern_counts[pattern_index] += count;
                pattern_files[pattern_index] += 1;
            }
        }

        let directory = normalized_directory_key(display_root, path.as_path());
        let entry = directory_counts.entry_ref(directory).or_default();
        entry.0 += 1;
        entry.1 += match_count;

        file_hotspots.push(FileHotspot {
            path: path.clone(),
            match_count,
            matching_lines,
            first_match_byte,
        });
        pattern_comparison_rows.push(PatternComparisonRow {
            path,
            total_matches: match_count,
            matching_lines,
            pattern_counts: file_pattern_counts,
        });
        record_extra(extra);
    }

    sort_frequencies!(file_hotspots, match_count, matching_lines; first_match_byte, path);
    sort_frequencies!(pattern_comparison_rows, total_matches, matching_lines; path);
    let mut directory_hotspots = directory_counts
        .into_iter()
        .map(|(path, (matched_files, match_count))| DirectoryHotspot {
            path,
            matched_files,
            match_count,
        })
        .collect::<Vec<_>>();
    sort_frequencies!(directory_hotspots, match_count, matched_files; path);

    ReportCore {
        root_is_dir: display_root.is_dir,
        root,
        stats,
        pattern_frequencies: build_pattern_frequencies(searcher, pattern_counts, pattern_files),
        file_hotspots,
        directory_hotspots,
        pattern_comparison_rows,
    }
}

fn build_pattern_frequencies(
    searcher: &Searcher,
    pattern_counts: Vec<usize>,
    pattern_files: Vec<usize>,
) -> Vec<PatternFrequency> {
    let mut frequencies = searcher
        .pattern_labels
        .iter()
        .enumerate()
        .map(|(pattern_index, pattern)| PatternFrequency {
            pattern_index,
            pattern: pattern.to_string(),
            match_count: pattern_counts[pattern_index],
            matched_files: pattern_files[pattern_index],
        })
        .collect::<Vec<_>>();
    sort_frequencies!(frequencies, match_count, matched_files; pattern_index);
    frequencies
}

fn count_matching_lines<M: ReportMatch>(bytes: &[u8], matches: &[M]) -> usize {
    let mut counter = crate::LineCounter::new(bytes);
    for matched in matches {
        counter.record(matched.start());
    }
    counter.count()
}

fn normalized_directory_key<'p>(root: DisplayRoot<'_>, file_path: &'p Path) -> &'p Path {
    match root.relative(file_path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        Some(_) | None => Path::new("."),
    }
}

/// A scanned root with its file kind resolved once, so display paths need no per-row `stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DisplayRoot<'a> {
    root: &'a Path,
    is_dir: bool,
}

impl<'a> DisplayRoot<'a> {
    /// Resolves whether `root` is a directory.
    pub(super) fn new(root: &'a Path) -> Self {
        Self::with_kind(root, root.is_dir())
    }

    /// Wraps `root` with an already-known file kind.
    pub(super) fn with_kind(root: &'a Path, is_dir: bool) -> Self {
        Self { root, is_dir }
    }

    /// Borrows the display path relative to a directory root, or the filename for a file root.
    pub(super) fn relative(self, path: &Path) -> &Path {
        if self.is_dir {
            path.strip_prefix(self.root).unwrap_or(path)
        } else {
            path.file_name().map(Path::new).unwrap_or(path)
        }
    }

    /// The text of [`Self::relative`], as `Path::display` would print it.
    ///
    /// Usually cut from the raw bytes; falls back to component-wise `strip_prefix` for paths
    /// the byte comparison cannot settle.
    pub(super) fn label(self, path: &Path) -> Cow<'_, str> {
        self.relative_text(path)
            .map_or_else(|| display_label(self.relative(path)), Cow::Borrowed)
    }

    /// The UTF-8 remainder of `path` below a directory root, when cutting the root's bytes
    /// off gives exactly what removing its components would.
    fn relative_text(self, path: &Path) -> Option<&str> {
        if !self.is_dir || std::path::MAIN_SEPARATOR != '/' {
            return None;
        }
        let root = self.root.as_os_str().as_encoded_bytes();
        let below = path.as_os_str().as_encoded_bytes().strip_prefix(root)?;
        let rest = if root.ends_with(b"/") {
            below
        } else {
            below.strip_prefix(b"/")?
        };
        // Defer to `strip_prefix` wherever component removal would differ from cutting bytes:
        // separators or `.` at either end, or an empty root.
        let plain = !rest.is_empty()
            && !root.is_empty()
            && !rest.starts_with(b"/")
            && !rest.starts_with(b"./")
            && rest != b"."
            && !rest.ends_with(b"/")
            && !rest.ends_with(b"/.");
        if plain {
            std::str::from_utf8(rest).ok()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SearchConfig, StructuredMode};

    #[test]
    fn directory_keys_named_like_a_relative_root_keep_their_label() {
        // Root `a` and file `a/a/f.txt` give key `a`; stripping the root again labelled it "".
        let root = DisplayRoot::with_kind(Path::new("a"), true);
        let key = normalized_directory_key(root, Path::new("a/a/f.txt"));
        let hotspots = [crate::DirectoryHotspot {
            path: key.to_path_buf(),
            matched_files: 1,
            match_count: 1,
        }];

        assert_eq!(key, Path::new("a"));
        assert_eq!(
            super::super::render::render_directory_density(&hotspots, 1),
            "Directory Match Density\na       1 matches  ########################"
        );
    }

    #[test]
    fn display_root_strips_directory_roots_and_names_file_roots() {
        let directory = DisplayRoot::with_kind(Path::new("logs"), true);
        let file = DisplayRoot::with_kind(Path::new("logs/app.log"), false);

        assert_eq!(
            directory.relative(Path::new("logs/a/app.log")),
            Path::new("a/app.log")
        );
        assert_eq!(
            directory.relative(Path::new("other/app.log")),
            Path::new("other/app.log")
        );
        assert_eq!(
            file.relative(Path::new("logs/app.log")),
            Path::new("app.log")
        );
        assert!(DisplayRoot::new(Path::new(env!("CARGO_MANIFEST_DIR"))).is_dir);
    }

    #[cfg(unix)]
    #[test]
    fn labels_cut_from_bytes_match_component_stripping() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let roots = [
            "", ".", "./", "./a", "a", "a/", "a//", "a/.", "a/..", "a/./b", "a/b", "/", "/a",
        ];
        let paths = [
            "a", "a/", "a/b", "a//b", "a/./b", "a/b/", "a/b/.", "a/b/c", "a/bc/x", "a/../b",
            "./a/b", "/a/b", "a/b/./c", "a/b//c", "a/./b/c", "b", ".", "a/.",
        ];
        for root in roots {
            for path in paths {
                let display = DisplayRoot::with_kind(Path::new(root), true);
                let path = Path::new(path);
                assert_eq!(
                    display.label(path),
                    display.relative(path).to_string_lossy(),
                    "root {root:?}, path {path:?}"
                );
            }
        }

        let invalid = Path::new(OsStr::from_bytes(b"logs/caf\xe9.log"));
        let directory = DisplayRoot::with_kind(Path::new("logs"), true);
        assert_eq!(directory.label(invalid), "caf\u{fffd}.log");
        let file = DisplayRoot::with_kind(Path::new("logs/app.log"), false);
        assert_eq!(file.label(Path::new("logs/app.log")), "app.log");
    }

    #[test]
    fn structured_counts_preserve_duplicate_hits_file_counts_and_tie_order() {
        assert!(
            MatchedFile::<()>::from_matches::<Match>(b"", 0, &[], || panic!(
                "empty extras must stay lazy"
            ))
            .is_none()
        );
        let config = SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .build();
        let searcher =
            Searcher::new(["hit", "$.field", "$.other"], config).expect("valid patterns");
        let mut accumulator = StructuredAccumulator::default();
        for document in [
            r#"{"field":"hit","other":"miss"}"#,
            r#"{"field":"miss","other":"hit"}"#,
            r#"{"field":"miss","other":"miss"}"#,
        ] {
            let matches = searcher.find_structured(document).expect("valid JSON");
            let matched = MatchedFile::from_matches(document.as_bytes(), 3, &matches, || {
                count_structured_values(&matches)
            })
            .expect("matching document");
            accumulator.record(matched.extra);
        }
        let (paths, values) = accumulator.finish();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].path, "$.field");
        assert_eq!(paths[1].path, "$.other");
        for path in paths {
            assert_eq!((path.match_count, path.matched_files), (4, 3));
            let entries = &values[&path.path];
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].value, "\"hit\"");
            assert_eq!(entries[1].value, "\"miss\"");
            assert_eq!((entries[0].match_count, entries[1].match_count), (2, 2));
        }
    }
}
