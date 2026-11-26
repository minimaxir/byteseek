use std::fs;
use std::path::{Path, PathBuf};

use byteseek::{PathOrder, SearchConfig, Searcher, StructuredMode};
use tempfile::TempDir;

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory should be created");
    }
    fs::write(path, contents).expect("fixture file should be written");
}

fn relative_paths(root: &Path, paths: impl IntoIterator<Item = PathBuf>) -> Vec<String> {
    let mut relative = paths
        .into_iter()
        .map(|path| {
            path.strip_prefix(root)
                .expect("path should be under test root")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    relative.sort();
    relative
}

#[test]
fn exact_file_search_respects_gitignore_by_default() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, ".gitignore", "ignored.txt\nignored_dir/\n");
    write_file(root, "visible.txt", "needle in a haystack");
    write_file(root, "ignored.txt", "needle in ignored file");
    write_file(
        root,
        "ignored_dir/nested.txt",
        "needle in nested ignored file",
    );

    let searcher =
        Searcher::new(["needle"], SearchConfig::default()).expect("searcher should build");
    let results = searcher
        .find_path(root)
        .expect("exact file search should succeed");

    let matched = relative_paths(root, results.into_iter().map(|result| result.path));
    assert_eq!(matched, vec!["visible.txt"]);
}

#[test]
fn exact_file_search_can_ignore_gitignore_rules() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, ".gitignore", "ignored.txt\nignored_dir/\n");
    write_file(root, "visible.txt", "needle in a haystack");
    write_file(root, "ignored.txt", "needle in ignored file");
    write_file(
        root,
        "ignored_dir/nested.txt",
        "needle in nested ignored file",
    );

    let config = SearchConfig::builder().ignore_gitignore(true).build();
    let searcher = Searcher::new(["needle"], config).expect("searcher should build");
    let results = searcher
        .find_path(root)
        .expect("exact file search should succeed");

    let matched = relative_paths(root, results.into_iter().map(|result| result.path));
    assert_eq!(
        matched,
        vec!["ignored.txt", "ignored_dir/nested.txt", "visible.txt"]
    );
}

#[test]
fn path_order_sorts_exact_results() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, "z-last.txt", "needle");
    write_file(root, "nested/b-middle.txt", "needle");
    write_file(root, "a-first.txt", "needle");

    let config = SearchConfig::builder().path_order(PathOrder::Path).build();
    let searcher = Searcher::new(["needle"], config).expect("searcher should build");
    let results = searcher
        .find_path(root)
        .expect("ordered exact file search should succeed");
    let paths = results
        .into_iter()
        .map(|result| {
            result
                .path
                .strip_prefix(root)
                .expect("path should be under test root")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        paths,
        vec!["a-first.txt", "nested/b-middle.txt", "z-last.txt"]
    );
}

#[test]
fn approximate_file_search_respects_gitignore_by_default() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, ".gitignore", "ignored.log\n");
    write_file(root, "visible.log", "EROR visible typo");
    write_file(root, "ignored.log", "EROR ignored typo");

    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = Searcher::new(["ERROR"], config).expect("searcher should build");
    let results = searcher
        .find_path(root)
        .expect("approximate file search should succeed");

    let matched = relative_paths(root, results.into_iter().map(|result| result.path));
    assert_eq!(matched, vec!["visible.log"]);
}

#[test]
fn structured_file_search_respects_gitignore_by_default() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, ".gitignore", "ignored.json\n");
    write_file(root, "visible.json", r#"{"user":{"name":"Alice"}}"#);
    write_file(root, "ignored.json", r#"{"user":{"name":"Bob"}}"#);

    let config = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Json))
        .build();
    let searcher = Searcher::new(["$.user.name"], config).expect("searcher should build");
    let results = searcher
        .find_structured_path(root)
        .expect("structured file search should succeed");

    let matched = relative_paths(root, results.iter().map(|result| result.path.clone()));
    assert_eq!(matched, vec!["visible.json"]);
    assert_eq!(results[0].matches[0].path, "$.user.name");
    assert_eq!(results[0].matches[0].node_text, "\"Alice\"");
}

#[test]
fn structured_file_search_can_ignore_gitignore_rules() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, ".gitignore", "ignored.json\n");
    write_file(root, "visible.json", r#"{"user":{"name":"Alice"}}"#);
    write_file(root, "ignored.json", r#"{"user":{"name":"Bob"}}"#);

    let config = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Json))
        .ignore_gitignore(true)
        .build();
    let searcher = Searcher::new(["$.user.name"], config).expect("searcher should build");
    let results = searcher
        .find_structured_path(root)
        .expect("structured file search should succeed");

    let matched = relative_paths(root, results.into_iter().map(|result| result.path));
    assert_eq!(matched, vec!["ignored.json", "visible.json"]);
}
