use std::fs;
use std::path::{Path, PathBuf};

use byteseek::{SearchConfig, SearchError, Searcher, StructuredMode};
use tempfile::TempDir;

const NOTES_TEXT: &str = "needle in text\n";
const DATA_JSON: &str = "{\"name\": \"needle\"}";
/// Valid JSON except for one non-UTF-8 byte.
const BINARY_JSON: &[u8] = b"{\"name\": \"\xff\"}";

/// Writes a file under `root`, creating parent directories.
fn write_file(root: &Path, relative: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory should be created");
    }
    fs::write(path, contents).expect("fixture file should be written");
}

/// Writes four files containing `needle`: a text file, a JSON file, and two non-UTF-8 files.
/// One of the latter has its invalid byte past the first chunk a search reads.
fn write_text_among_binaries(root: &Path) {
    let mut late = b"needle ".repeat(10_000);
    late.push(0xff);
    write_file(root, "notes.txt", NOTES_TEXT);
    write_file(root, "data.json", DATA_JSON);
    write_file(root, "image.bin", b"needle \xff\xfe");
    write_file(root, "nested/archive.bin", late);
}

/// Sorted file names of `paths`.
fn file_names(paths: impl IntoIterator<Item = PathBuf>) -> Vec<String> {
    let mut names: Vec<String> = paths
        .into_iter()
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names
}

/// Whether `error` is a failed read of a path ending in `name`.
fn names_unread_file(error: &SearchError, name: &str) -> bool {
    matches!(error, SearchError::FileRead { path, .. } if path.ends_with(name))
}

/// A JSON searcher for `$.name`.
fn json_searcher() -> Searcher {
    let config = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Json))
        .build();
    Searcher::new(["$.name"], config).expect("a path pattern compiles")
}

#[test]
fn directory_search_passes_over_files_that_are_not_utf8() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    write_text_among_binaries(tempdir.path());
    let one_edit = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searchers = [
        Searcher::new(["needle"], SearchConfig::default()),
        Searcher::new(["nedle"], one_edit),
    ];
    for searcher in searchers {
        let searcher = searcher.expect("test searchers compile");

        let results = searcher
            .find_path(tempdir.path())
            .expect("a directory holding binaries should be searched");

        let matched = file_names(results.into_iter().map(|result| result.path));
        assert_eq!(matched, ["data.json", "notes.txt"], "{searcher:?}");
    }
}

#[test]
fn structured_directory_search_passes_over_files_that_are_not_utf8() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();
    write_file(root, "data.json", DATA_JSON);
    write_file(root, "image.bin", BINARY_JSON);

    let results = json_searcher()
        .find_structured_path(root)
        .expect("a directory holding a binary should be searched");

    let matched = file_names(results.into_iter().map(|result| result.path));
    assert_eq!(matched, ["data.json"]);
}

#[test]
fn a_file_that_is_not_utf8_fails_when_named_itself() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();
    write_text_among_binaries(root);
    let searcher =
        Searcher::new(["needle"], SearchConfig::default()).expect("searcher should build");

    let early = searcher.find_path(root.join("image.bin"));
    let late = searcher.find_path(root.join("nested/archive.bin"));
    let structured = json_searcher().find_structured_path(root.join("image.bin"));

    assert!(early.is_err_and(|error| names_unread_file(&error, "image.bin")));
    assert!(late.is_err_and(|error| names_unread_file(&error, "archive.bin")));
    assert!(structured.is_err_and(|error| names_unread_file(&error, "image.bin")));
}

#[test]
fn reports_scan_only_the_utf8_files_of_a_directory() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();
    write_text_among_binaries(root);
    let searcher =
        Searcher::new(["needle"], SearchConfig::default()).expect("searcher should build");

    let report = searcher
        .report_path(root)
        .expect("a directory holding binaries should be reported on");

    assert_eq!(report.stats().scanned_files, 2);
    assert_eq!(
        report.stats().scanned_bytes,
        (NOTES_TEXT.len() + DATA_JSON.len()) as u64
    );
    assert_eq!(report.stats().total_matches, 2);
}

#[test]
fn structured_reports_scan_only_the_utf8_files_of_a_directory() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();
    write_file(root, "data.json", DATA_JSON);
    write_file(root, "image.bin", BINARY_JSON);

    let report = json_searcher()
        .structured_report_path(root)
        .expect("a directory holding a binary should be reported on");

    assert_eq!(report.stats().scanned_files, 1);
    assert_eq!(report.stats().matched_files, 1);
}
