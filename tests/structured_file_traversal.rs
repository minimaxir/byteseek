use std::fs;
use std::path::Path;

use byteseek::{
    OwnedStructuredMatch, SearchConfig, Searcher, StructuredFileMatchResult, StructuredMatchKind,
    StructuredMode,
};
use tempfile::TempDir;

type MatchProjection = (usize, String, String, StructuredMatchKind);
type FileProjection = (String, Vec<MatchProjection>);

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory should be created");
    }
    fs::write(path, contents).expect("fixture file should be written");
}

fn build_structured_searcher(
    mode: StructuredMode,
    patterns: &[&str],
    ignore_gitignore: bool,
) -> Searcher {
    let config = SearchConfig::builder()
        .structured_mode(Some(mode))
        .ignore_gitignore(ignore_gitignore)
        .build();
    Searcher::new(patterns.iter().copied(), config).expect("searcher should build")
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("path should be under test root")
        .to_string_lossy()
        .replace('\\', "/")
}

fn match_projection(matches: Vec<OwnedStructuredMatch>) -> Vec<MatchProjection> {
    let mut projection = matches
        .into_iter()
        .map(|matched| {
            (
                matched.pattern_index,
                matched.path,
                matched.node_text,
                matched.match_kind,
            )
        })
        .collect::<Vec<_>>();
    projection.sort_unstable_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    projection
}

fn sort_results(root: &Path, mut results: Vec<StructuredFileMatchResult>) -> Vec<FileProjection> {
    results.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    results
        .into_iter()
        .map(|result| {
            (
                relative_path(root, &result.path),
                match_projection(result.matches),
            )
        })
        .collect()
}

#[test]
fn structured_json_traversal_respects_nested_gitignores() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, ".gitignore", "archive/\n");
    write_file(root, "services/.gitignore", "local/\n");
    write_file(
        root,
        "services/payments/service.json",
        r#"{
  "service": {"name": "payments-api"},
  "alerts": [
    {"severity": "warning"},
    {"severity": "critical"}
  ]
}"#,
    );
    write_file(
        root,
        "services/orders/service.json",
        r#"{
  "service": {"name": "orders-api"},
  "alerts": [
    {"severity": "info"},
    {"severity": "warning"}
  ]
}"#,
    );
    write_file(
        root,
        "services/local/override.json",
        r#"{
  "service": {"name": "payments-api"},
  "alerts": [
    {"severity": "info"},
    {"severity": "critical"}
  ]
}"#,
    );
    write_file(root, "archive/broken.json", "{\"service\":");

    let searcher = build_structured_searcher(
        StructuredMode::Json,
        &[
            "$.service.name=payments-api",
            "$.alerts[1].severity=critical",
        ],
        false,
    );
    let results = searcher
        .find_structured_path(root)
        .expect("structured traversal should skip ignored files");

    assert_eq!(
        sort_results(root, results),
        vec![(
            "services/payments/service.json".to_string(),
            vec![
                (
                    0,
                    "$.service.name".to_string(),
                    "\"payments-api\"".to_string(),
                    StructuredMatchKind::PathValue,
                ),
                (
                    1,
                    "$.alerts[1].severity".to_string(),
                    "\"critical\"".to_string(),
                    StructuredMatchKind::PathValue,
                ),
            ],
        )]
    );
}

#[test]
fn structured_json_traversal_can_include_nested_gitignored_files() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, ".gitignore", "archive/\n");
    write_file(root, "services/.gitignore", "local/\n");
    write_file(
        root,
        "services/payments/service.json",
        r#"{"service":{"name":"payments-api"},"alerts":[{"severity":"info"},{"severity":"critical"}]}"#,
    );
    write_file(
        root,
        "services/local/override.json",
        r#"{"service":{"name":"payments-api"},"alerts":[{"severity":"info"},{"severity":"critical"}]}"#,
    );
    write_file(
        root,
        "archive/snapshot.json",
        r#"{"service":{"name":"payments-api"},"alerts":[{"severity":"info"},{"severity":"critical"}]}"#,
    );

    let searcher = build_structured_searcher(
        StructuredMode::Json,
        &[
            "$.service.name=payments-api",
            "$.alerts[1].severity=critical",
        ],
        true,
    );
    let results = searcher
        .find_structured_path(root)
        .expect("structured traversal should include ignored files when configured");

    let projected = sort_results(root, results);
    let matched_paths = projected
        .iter()
        .map(|(path, _)| path.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        matched_paths,
        vec![
            "archive/snapshot.json",
            "services/local/override.json",
            "services/payments/service.json",
        ]
    );
    for (_, matches) in projected {
        assert_eq!(
            matches,
            vec![
                (
                    0,
                    "$.service.name".to_string(),
                    "\"payments-api\"".to_string(),
                    StructuredMatchKind::PathValue,
                ),
                (
                    1,
                    "$.alerts[1].severity".to_string(),
                    "\"critical\"".to_string(),
                    StructuredMatchKind::PathValue,
                ),
            ]
        );
    }
}

#[test]
fn structured_yaml_traversal_matches_multi_document_manifests() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, ".gitignore", "generated/\n");
    write_file(root, "clusters/prod/.gitignore", "scratch.yaml\n");
    write_file(
        root,
        "clusters/prod/workloads.yaml",
        r#"apiVersion: v1
kind: Service
metadata:
  name: payments
  labels:
    app.kubernetes.io/part-of: payments
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: payments
spec:
  template:
    spec:
      containers:
        - name: api
          image: registry.example.com/payments:2026.03.1
"#,
    );
    write_file(
        root,
        "clusters/staging/workloads.yaml",
        r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: staging-payments
spec:
  template:
    spec:
      containers:
        - name: api
          image: registry.example.com/payments:2026.03.0
"#,
    );
    write_file(
        root,
        "clusters/prod/scratch.yaml",
        r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: ignored
spec:
  template:
    spec:
      containers:
        - name: api
          image: registry.example.com/payments:2026.03.1
"#,
    );
    write_file(
        root,
        "generated/bad.yaml",
        "apiVersion: v1\nmetadata: [broken\n",
    );

    let searcher = build_structured_searcher(
        StructuredMode::Yaml,
        &[
            "$[0].metadata.labels[\"app.kubernetes.io/part-of\"]=payments",
            "$[1].spec.template.spec.containers[0].image=registry.example.com/payments:2026.03.1",
        ],
        false,
    );
    let results = searcher
        .find_structured_path(root)
        .expect("structured traversal should succeed for visible YAML");

    assert_eq!(
        sort_results(root, results),
        vec![(
            "clusters/prod/workloads.yaml".to_string(),
            vec![
                (
                    0,
                    "$[0].metadata.labels[\"app.kubernetes.io/part-of\"]".to_string(),
                    "payments".to_string(),
                    StructuredMatchKind::PathValue,
                ),
                (
                    1,
                    "$[1].spec.template.spec.containers[0].image".to_string(),
                    "registry.example.com/payments:2026.03.1".to_string(),
                    StructuredMatchKind::PathValue,
                ),
            ],
        )]
    );
}

#[test]
fn structured_toml_traversal_matches_nested_project_configs() {
    let tempdir = TempDir::new().expect("temporary directory should be created");
    let root = tempdir.path();

    write_file(root, ".gitignore", "scratch/\n");
    write_file(root, "services/payments/.gitignore", "generated.toml\n");
    write_file(
        root,
        "services/payments/pyproject.toml",
        r#"[project]
name = "billing-worker"

[tool.ruff.lint]
select = ["E", "F", "B"]

[[tool.uv.index]]
name = "internal"
url = "https://packages.example.com/simple"

[[tool.uv.index]]
name = "backup"
url = "https://mirror.example.com/simple"
"#,
    );
    write_file(
        root,
        "services/orders/pyproject.toml",
        r#"[project]
name = "orders-worker"

[tool.ruff.lint]
select = ["E", "F"]
"#,
    );
    write_file(
        root,
        "services/payments/generated.toml",
        r#"[project]
name = "generated-worker"
"#,
    );
    write_file(root, "scratch/bad.toml", "[project\nname = \"broken\"");

    let searcher = build_structured_searcher(
        StructuredMode::Toml,
        &[
            "$.project.name=billing-worker",
            "$.tool.ruff.lint.select[2]=B",
            "$.tool.uv.index[1].url",
        ],
        false,
    );
    let results = searcher
        .find_structured_path(root)
        .expect("structured traversal should succeed for visible TOML");

    assert_eq!(
        sort_results(root, results),
        vec![(
            "services/payments/pyproject.toml".to_string(),
            vec![
                (
                    0,
                    "$.project.name".to_string(),
                    "\"billing-worker\"".to_string(),
                    StructuredMatchKind::PathValue,
                ),
                (
                    1,
                    "$.tool.ruff.lint.select[2]".to_string(),
                    "\"B\"".to_string(),
                    StructuredMatchKind::PathValue,
                ),
                (
                    2,
                    "$.tool.uv.index[1].url".to_string(),
                    "\"https://mirror.example.com/simple\"".to_string(),
                    StructuredMatchKind::Path,
                ),
            ],
        )]
    );
}
