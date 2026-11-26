use std::fs;
use std::path::Path;

use byteseek::{CaseMode, SearchConfig, SearchError, Searcher, StructuredMode};
use tempfile::TempDir;

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory should be created");
    }
    fs::write(path, contents).expect("fixture file should be written");
}

#[test]
fn exact_report_builds_summary_tables_and_hotspots() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, ".gitignore", "ignored.log\n");
    write_file(root, "root.log", "ERROR timeout\nstill ERROR\n");
    write_file(root, "nested/api.log", "timeout while calling upstream\n");
    write_file(root, "ignored.log", "ERROR should be ignored\n");

    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let searcher = Searcher::new(["error", "timeout"], config).expect("searcher should build");
    let report = searcher
        .report_path(root)
        .expect("report generation should succeed");

    assert_eq!(report.stats().scanned_files, 2);
    assert_eq!(report.stats().matched_files, 2);
    assert_eq!(report.stats().total_matches, 4);
    assert_eq!(report.stats().total_matching_lines, 3);

    let file_hotspots = report.file_hotspots();
    assert_eq!(file_hotspots.len(), 2);
    assert_eq!(
        file_hotspots[0]
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("file name should be valid UTF-8"),
        "root.log"
    );
    assert_eq!(file_hotspots[0].match_count, 3);

    let pattern_frequencies = report.pattern_frequencies();
    assert_eq!(pattern_frequencies[0].pattern, "timeout");
    assert_eq!(pattern_frequencies[0].match_count, 2);
    assert_eq!(pattern_frequencies[0].matched_files, 2);
    assert_eq!(pattern_frequencies[1].pattern, "error");
    assert_eq!(pattern_frequencies[1].match_count, 2);

    let comparison_rows = report.pattern_comparison_rows();
    assert_eq!(comparison_rows.len(), 2);
    assert_eq!(comparison_rows[0].total_matches, 3);
    assert_eq!(comparison_rows[0].pattern_counts, vec![2, 1]);
    assert_eq!(comparison_rows[1].pattern_counts, vec![0, 1]);

    let summary = report.render_summary();
    assert!(summary.contains("Search Summary"));
    assert!(summary.contains("Scanned: 2 files"));
    assert!(summary.contains("Matched: 2 files, 4 matches, 3 matching lines"));

    let files_table = report.render_top_files_table(10);
    assert!(files_table.contains("Top Matching Files"));
    assert!(files_table.contains("root.log"));
    assert!(files_table.contains("nested/api.log"));

    let density = report.render_directory_density(10);
    assert!(density.contains("Directory Match Density"));
    assert!(density.contains("."));
    assert!(density.contains("nested"));

    let pattern_table = report.render_pattern_frequency_table();
    assert!(pattern_table.contains("Pattern Frequencies"));
    assert!(pattern_table.contains("error"));
    assert!(pattern_table.contains("timeout"));

    let comparison_table = report.render_pattern_comparison_table(10, 2);
    assert!(comparison_table.contains("Multi-Pattern File Comparison"));
    assert!(comparison_table.contains("root.log"));
    assert!(comparison_table.contains("nested/api.log"));
    assert!(comparison_table.contains("SHOWN TOTAL"));

    let cooccurrence = report.render_pattern_cooccurrence_matrix(2);
    assert!(cooccurrence.contains("Pattern File Co-occurrence"));
    assert!(cooccurrence.contains("error"));
    assert!(cooccurrence.contains("timeout"));
    assert!(cooccurrence.contains("1/1"));
    assert!(cooccurrence.contains("1/2"));
}

#[test]
fn structured_report_tracks_paths_and_value_frequencies() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(
        root,
        "events-a.json",
        r#"{"service":"payments","status":"fail","env":"prod"}"#,
    );
    write_file(
        root,
        "nested/events-b.json",
        r#"{"service":"payments","status":"fail","env":"prod"}"#,
    );
    write_file(
        root,
        "nested/events-c.json",
        r#"{"service":"auth","status":"ok","env":"staging"}"#,
    );

    let config = SearchConfig::builder()
        .structured_mode(Some(StructuredMode::Json))
        .build();
    let searcher = Searcher::new(["payments", "$.status", "$.env"], config)
        .expect("structured searcher should build");
    let report = searcher
        .structured_report_path(root)
        .expect("structured report generation should succeed");

    assert_eq!(report.stats().scanned_files, 3);
    assert_eq!(report.stats().matched_files, 3);
    assert_eq!(report.stats().total_matches, 8);

    let path_frequencies = report.path_frequencies();
    assert_eq!(path_frequencies[0].path, "$.env");
    assert_eq!(path_frequencies[0].match_count, 3);

    let status_values = report
        .value_frequencies_for_path("$.status")
        .expect("status path frequencies should exist");
    assert_eq!(status_values[0].value, "\"fail\"");
    assert_eq!(status_values[0].match_count, 2);
    assert_eq!(status_values[1].value, "\"ok\"");
    assert_eq!(status_values[1].match_count, 1);

    let comparison_rows = report.pattern_comparison_rows();
    assert_eq!(comparison_rows.len(), 3);
    assert_eq!(comparison_rows[0].pattern_counts, vec![1, 1, 1]);
    assert_eq!(comparison_rows[2].pattern_counts, vec![0, 1, 1]);

    let summary = report.render_summary();
    assert!(summary.contains("Structured Search Summary"));
    assert!(summary.contains("Matched: 3 files, 8 matches"));

    let path_table = report.render_path_frequency_table(10);
    assert!(path_table.contains("Structured Path Frequencies"));
    assert!(path_table.contains("$.env"));
    assert!(path_table.contains("$.service"));
    assert!(path_table.contains("$.status"));

    let value_table = report
        .render_value_frequency_table("$.status", 10)
        .expect("status value table should render");
    assert!(value_table.contains("Field Value Summary: $.status"));
    assert!(value_table.contains("\"fail\""));
    assert!(value_table.contains("\"ok\""));

    let comparison_table = report.render_pattern_comparison_table(10, 3);
    assert!(comparison_table.contains("Multi-Pattern File Comparison"));
    assert!(comparison_table.contains("events-a.json"));
    assert!(comparison_table.contains("nested/events-c.json"));
    assert!(comparison_table.contains("SHOWN TOTAL"));

    let cooccurrence = report.render_pattern_cooccurrence_matrix(3);
    assert!(cooccurrence.contains("Pattern File Co-occurrence"));
    assert!(cooccurrence.contains("payments"));
    assert!(cooccurrence.contains("$.status"));
    assert!(cooccurrence.contains("$.env"));
    assert!(cooccurrence.contains("2/2"));
    assert!(cooccurrence.contains("2/3"));
}

#[test]
fn structured_report_requires_structured_mode() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();
    write_file(root, "plain.txt", "hello world");

    let searcher =
        Searcher::new(["hello"], SearchConfig::default()).expect("searcher should build");
    let error = searcher
        .structured_report_path(root)
        .expect_err("structured report should require structured mode");

    assert_eq!(error, SearchError::StructuredModeDisabled);
}
