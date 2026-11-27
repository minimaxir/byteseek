use std::cell::Cell;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Duration;

use byteseek::{SearchConfig, Searcher, StructuredMode};
use criterion::{Bencher, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tempfile::TempDir;

const BENCH_VARIANT_COUNT: usize = 16;
const TENANT_COUNT: usize = 2;
const SERVICE_NAMES: [&str; 3] = ["auth", "billing", "search"];
const REGIONS: [&str; 2] = ["us-west", "eu-central"];
const SHARD_COUNT: usize = 2;

#[derive(Clone, Copy)]
enum StructuredFormat {
    Json,
    Yaml,
    Toml,
}

struct TraversalFixture {
    _roots: Vec<TempDir>,
    paths: Vec<PathBuf>,
    visible_files: usize,
    total_files: usize,
    visible_bytes: u64,
    total_bytes: u64,
}

impl TraversalFixture {
    fn visible_throughput(&self) -> Throughput {
        Throughput::Bytes(self.visible_bytes)
    }

    fn total_throughput(&self) -> Throughput {
        Throughput::Bytes(self.total_bytes)
    }
}

fn benchmark_exact_traversal(c: &mut Criterion) {
    let fixture = build_text_fixture();
    let searcher = Searcher::new(
        ["ERROR", "status=500", "service=billing"],
        SearchConfig::default(),
    )
    .expect("exact traversal searcher should build");
    let include_ignored_searcher = Searcher::new(
        ["ERROR", "status=500", "service=billing"],
        SearchConfig::builder().ignore_gitignore(true).build(),
    )
    .expect("exact traversal searcher should build");

    let mut group = c.benchmark_group("file_traversal_exact");
    group.throughput(fixture.visible_throughput());
    group.bench_with_input(
        BenchmarkId::new("respect_gitignore", fixture.visible_files),
        &fixture,
        |bench, current_fixture| {
            bench_independent_roots(bench, current_fixture, |root| {
                black_box(
                    searcher
                        .find_path(root)
                        .expect("exact traversal benchmark search should succeed"),
                );
            });
        },
    );

    group.throughput(fixture.total_throughput());
    group.bench_with_input(
        BenchmarkId::new("ignore_gitignore", fixture.total_files),
        &fixture,
        |bench, current_fixture| {
            bench_independent_roots(bench, current_fixture, |root| {
                black_box(
                    include_ignored_searcher
                        .find_path(root)
                        .expect("exact traversal benchmark search should succeed"),
                );
            });
        },
    );
    group.finish();
}

fn benchmark_approximate_traversal(c: &mut Criterion) {
    let fixture = build_text_fixture();
    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = Searcher::new(["EROR", "sttus=500", "servce=billing"], config)
        .expect("approximate traversal searcher should build");
    let include_ignored_searcher = Searcher::new(
        ["EROR", "sttus=500", "servce=billing"],
        SearchConfig::builder()
            .max_edit_distance(Some(1))
            .ignore_gitignore(true)
            .build(),
    )
    .expect("approximate traversal searcher should build");

    let mut group = c.benchmark_group("file_traversal_approximate");
    group.throughput(fixture.visible_throughput());
    group.bench_with_input(
        BenchmarkId::new("respect_gitignore", fixture.visible_files),
        &fixture,
        |bench, current_fixture| {
            bench_independent_roots(bench, current_fixture, |root| {
                black_box(
                    searcher
                        .find_path(root)
                        .expect("approximate traversal benchmark search should succeed"),
                );
            });
        },
    );

    group.throughput(fixture.total_throughput());
    group.bench_with_input(
        BenchmarkId::new("ignore_gitignore", fixture.total_files),
        &fixture,
        |bench, current_fixture| {
            bench_independent_roots(bench, current_fixture, |root| {
                black_box(
                    include_ignored_searcher
                        .find_path(root)
                        .expect("approximate traversal benchmark search should succeed"),
                );
            });
        },
    );
    group.finish();
}

fn benchmark_structured_json_traversal(c: &mut Criterion) {
    benchmark_structured_format_traversal(c, StructuredFormat::Json);
}

fn benchmark_structured_yaml_traversal(c: &mut Criterion) {
    benchmark_structured_format_traversal(c, StructuredFormat::Yaml);
}

fn benchmark_structured_toml_traversal(c: &mut Criterion) {
    benchmark_structured_format_traversal(c, StructuredFormat::Toml);
}

fn benchmark_structured_format_traversal(c: &mut Criterion, format: StructuredFormat) {
    let fixture = build_structured_fixture(format);
    let config = SearchConfig::builder()
        .structured_mode(Some(structured_mode(format)))
        .build();
    let searcher = Searcher::new(
        [
            "$.tenant.service.metadata.owner.team",
            "$.tenant.service.summary.alerts[1].status",
            "$.tenant.service.summary.alerts[1].status=failed",
            "platform",
        ],
        config,
    )
    .expect("structured traversal searcher should build");
    let include_ignored_searcher = Searcher::new(
        [
            "$.tenant.service.metadata.owner.team",
            "$.tenant.service.summary.alerts[1].status",
            "$.tenant.service.summary.alerts[1].status=failed",
            "platform",
        ],
        SearchConfig::builder()
            .structured_mode(Some(structured_mode(format)))
            .ignore_gitignore(true)
            .build(),
    )
    .expect("structured traversal searcher should build");

    let mut group = c.benchmark_group(group_name(format));
    group.throughput(fixture.visible_throughput());
    group.bench_with_input(
        BenchmarkId::new("respect_gitignore", fixture.visible_files),
        &fixture,
        |bench, current_fixture| {
            bench_independent_roots(bench, current_fixture, |root| {
                black_box(
                    searcher
                        .find_structured_path(root)
                        .expect("structured traversal benchmark search should succeed"),
                );
            });
        },
    );

    group.throughput(fixture.total_throughput());
    group.bench_with_input(
        BenchmarkId::new("ignore_gitignore", fixture.total_files),
        &fixture,
        |bench, current_fixture| {
            bench_independent_roots(bench, current_fixture, |root| {
                black_box(
                    include_ignored_searcher
                        .find_structured_path(root)
                        .expect("structured traversal benchmark search should succeed"),
                );
            });
        },
    );
    group.finish();
}

fn structured_mode(format: StructuredFormat) -> StructuredMode {
    match format {
        StructuredFormat::Json => StructuredMode::Json,
        StructuredFormat::Yaml => StructuredMode::Yaml,
        StructuredFormat::Toml => StructuredMode::Toml,
    }
}

fn group_name(format: StructuredFormat) -> &'static str {
    match format {
        StructuredFormat::Json => "file_traversal_structured_json",
        StructuredFormat::Yaml => "file_traversal_structured_yaml",
        StructuredFormat::Toml => "file_traversal_structured_toml",
    }
}

fn bench_independent_roots(
    bench: &mut Bencher<'_>,
    fixture: &TraversalFixture,
    mut run: impl FnMut(&Path),
) {
    let next = Cell::new(0_usize);
    bench.iter(|| {
        let index = next.get();
        let next_index = if index + 1 == fixture.paths.len() {
            0
        } else {
            index + 1
        };
        next.set(next_index);
        run(black_box(fixture.paths[index].as_path()));
    });
}

fn build_text_fixture() -> TraversalFixture {
    build_fixture(|leaf, variant| {
        let visible = build_text_log(leaf, variant, false);
        let ignored = build_text_log(leaf, variant, true);
        (visible, ignored, "log")
    })
}

fn build_structured_fixture(format: StructuredFormat) -> TraversalFixture {
    build_fixture(|leaf, variant| match format {
        StructuredFormat::Json => (
            build_json_document(leaf, variant, false),
            build_json_document(leaf, variant, true),
            "json",
        ),
        StructuredFormat::Yaml => (
            build_yaml_document(leaf, variant, false),
            build_yaml_document(leaf, variant, true),
            "yaml",
        ),
        StructuredFormat::Toml => (
            build_toml_document(leaf, variant, false),
            build_toml_document(leaf, variant, true),
            "toml",
        ),
    })
}

fn build_fixture(
    build_contents: impl Fn(LeafSpec<'_>, usize) -> (String, String, &'static str),
) -> TraversalFixture {
    let mut roots = Vec::with_capacity(BENCH_VARIANT_COUNT);
    let mut paths = Vec::with_capacity(BENCH_VARIANT_COUNT);
    let mut visible_bytes = 0_u64;
    let mut total_bytes = 0_u64;
    let mut visible_files = 0_usize;
    let mut total_files = 0_usize;

    for variant in 0..BENCH_VARIANT_COUNT {
        let root = TempDir::new().expect("benchmark tempdir should build");
        write_text_file(
            root.path().join(".gitignore").as_path(),
            "ignored/\nvendor/\n*.tmp\n",
        );
        let (_, _, extension) = build_contents(
            LeafSpec {
                tenant: 0,
                service_index: 0,
                service_name: SERVICE_NAMES[0],
                region_index: 0,
                region: REGIONS[0],
                shard: 0,
            },
            variant,
        );

        let mut variant_visible_bytes = 0_u64;
        let mut variant_total_bytes = 0_u64;
        let mut variant_visible_files = 0_usize;
        let mut variant_total_files = 0_usize;

        for tenant in 0..TENANT_COUNT {
            for (service_index, service_name) in SERVICE_NAMES.iter().enumerate() {
                for (region_index, region) in REGIONS.iter().enumerate() {
                    for shard in 0..SHARD_COUNT {
                        let leaf = LeafSpec {
                            tenant,
                            service_index,
                            service_name,
                            region_index,
                            region,
                            shard,
                        };
                        let (visible, ignored, extension) = build_contents(leaf, variant);
                        let relative_stem = format!(
                            "workspace/tenant-{tenant:02}/service-{service_name}/region-{region}/\
                             batch-{region_index:02}/shard-{shard:02}/slice-{service_index:02}"
                        );
                        let visible_path = root
                            .path()
                            .join(relative_stem.as_str())
                            .join(format!("report.{extension}"));
                        write_text_file(visible_path.as_path(), visible.as_str());

                        let ignored_path = root
                            .path()
                            .join("ignored")
                            .join(relative_stem.as_str())
                            .join(format!("shadow.{extension}.tmp"));
                        write_text_file(ignored_path.as_path(), ignored.as_str());

                        variant_visible_bytes += visible.len() as u64;
                        variant_total_bytes += (visible.len() + ignored.len()) as u64;
                        variant_visible_files += 1;
                        variant_total_files += 2;
                    }
                }
            }
        }

        let vendor_path = root
            .path()
            .join("vendor/mirrors/archive/deep/nesting")
            .join(format!("third_party.{extension}"));
        let vendor_contents = build_vendor_stub(extension, variant);
        write_text_file(vendor_path.as_path(), vendor_contents.as_str());
        variant_total_bytes += vendor_contents.len() as u64;
        variant_total_files += 1;

        if variant == 0 {
            visible_bytes = variant_visible_bytes;
            total_bytes = variant_total_bytes;
            visible_files = variant_visible_files;
            total_files = variant_total_files;
        }

        paths.push(root.path().to_path_buf());
        roots.push(root);
    }

    TraversalFixture {
        _roots: roots,
        paths,
        visible_files,
        total_files,
        visible_bytes,
        total_bytes,
    }
}

fn write_text_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("benchmark directory tree should build");
    }
    fs::write(path, contents).expect("benchmark file should write");
}

fn build_vendor_stub(extension: &str, variant: usize) -> String {
    match extension {
        "json" => format!(
            "{{\"tenant\":{{\"service\":{{\"metadata\":{{\"owner\":{{\"team\":\"platform\"}}}},\
             \"summary\":{{\"alerts\":[{{\"name\":\"latency\",\"status\":\"ok\"}},\
             {{\"name\":\"replication\",\"status\":\"failed\"}}]}}}}}},\"variant\":{variant}}}\n"
        ),
        "yaml" => format!(
            "tenant:\n  service:\n    metadata:\n      owner:\n        team: platform\n    \
             summary:\n      alerts:\n        - name: latency\n          status: ok\n        - \
             name: replication\n          status: failed\nvariant: {variant}\n"
        ),
        "toml" => format!(
            "[tenant.service.metadata.owner]\nteam = \"platform\"\n\n\
             [[tenant.service.summary.alerts]]\nname = \"latency\"\nstatus = \"ok\"\n\n\
             [[tenant.service.summary.alerts]]\nname = \"replication\"\nstatus = \"failed\"\n\n\
             variant = {variant}\n"
        ),
        _ => {
            format!(
                "vendor-only sentinel\nvariant={variant:02}\nERROR status=500 service=billing\n"
            )
        }
    }
}

#[derive(Clone, Copy)]
struct LeafSpec<'a> {
    tenant: usize,
    service_index: usize,
    service_name: &'a str,
    region_index: usize,
    region: &'a str,
    shard: usize,
}

fn build_text_log(leaf: LeafSpec<'_>, variant: usize, ignored: bool) -> String {
    let mut output = String::with_capacity(1_536);
    for entry in 0..18 {
        let level = if entry % 5 == 0 { "ERROR" } else { "INFO" };
        let status = if entry % 6 == 0 { "500" } else { "200" };
        output.push_str("2026-02-");
        push_two_digits(&mut output, (leaf.tenant + 1) as u8);
        output.push_str("T12:");
        push_two_digits(&mut output, (entry * 3) as u8);
        output.push(':');
        push_two_digits(&mut output, (entry * 7) as u8);
        output.push(' ');
        output.push_str(level);
        output.push_str(" tenant=");
        output.push_str(leaf.tenant.to_string().as_str());
        output.push_str(" service=");
        output.push_str(leaf.service_name);
        output.push_str(" region=");
        output.push_str(leaf.region);
        output.push_str(" shard=");
        output.push_str(leaf.shard.to_string().as_str());
        output.push_str(" status=");
        output.push_str(status);
        output.push_str(" message=");
        output.push_str(if ignored {
            "ignored traversal sentinel"
        } else {
            "visible traversal sentinel"
        });
        output.push('\n');
    }

    output.push_str("variant=");
    push_two_digits(&mut output, variant as u8);
    output.push_str(" service-index=");
    push_two_digits(&mut output, leaf.service_index as u8);
    output.push_str(" region-index=");
    push_two_digits(&mut output, leaf.region_index as u8);
    output.push('\n');
    output
}

fn build_json_document(leaf: LeafSpec<'_>, variant: usize, ignored: bool) -> String {
    let mut output = String::with_capacity(1_280);
    output.push_str("{\n");
    output.push_str("  \"tenant\": {\n");
    output.push_str("    \"id\": \"tenant-");
    push_two_digits(&mut output, leaf.tenant as u8);
    output.push_str("\",\n");
    output.push_str("    \"service\": {\n");
    output.push_str("      \"name\": \"");
    output.push_str(leaf.service_name);
    output.push_str("\",\n");
    output.push_str("      \"region\": \"");
    output.push_str(leaf.region);
    output.push_str("\",\n");
    output.push_str("      \"metadata\": {\n");
    output.push_str("        \"owner\": {\n");
    output.push_str("          \"team\": \"platform\",\n");
    output.push_str("          \"rotation\": \"");
    output.push_str(if ignored { "shadow" } else { "primary" });
    output.push_str("\"\n");
    output.push_str("        }\n");
    output.push_str("      },\n");
    output.push_str("      \"summary\": {\n");
    output.push_str("        \"alerts\": [\n");
    output.push_str("          {\"name\": \"latency\", \"status\": \"ok\"},\n");
    output.push_str("          {\"name\": \"replication\", \"status\": \"failed\"}\n");
    output.push_str("        ]\n");
    output.push_str("      }\n");
    output.push_str("    }\n");
    output.push_str("  },\n");
    output.push_str("  \"deployment\": {\n");
    output.push_str("    \"layers\": {\n");
    output.push_str("      \"batch\": ");
    output.push_str(leaf.region_index.to_string().as_str());
    output.push_str(",\n");
    output.push_str("      \"shard\": ");
    output.push_str(leaf.shard.to_string().as_str());
    output.push('\n');
    output.push_str("    }\n");
    output.push_str("  }\n");
    output.push_str("}\n");
    append_json_variant_suffix(&mut output, variant);
    output
}

fn build_yaml_document(leaf: LeafSpec<'_>, variant: usize, ignored: bool) -> String {
    let mut output = String::with_capacity(1_280);
    output.push_str("tenant:\n");
    output.push_str("  id: tenant-");
    push_two_digits(&mut output, leaf.tenant as u8);
    output.push_str("\n  service:\n");
    output.push_str("    name: ");
    output.push_str(leaf.service_name);
    output.push_str("\n    region: ");
    output.push_str(leaf.region);
    output.push_str("\n    metadata:\n      owner:\n        team: platform\n        rotation: ");
    output.push_str(if ignored { "shadow" } else { "primary" });
    output.push_str("\n    summary:\n      alerts:\n");
    output.push_str("        - name: latency\n          status: ok\n");
    output.push_str("        - name: replication\n          status: failed\n");
    output.push_str("deployment:\n  layers:\n");
    output.push_str("    batch: ");
    output.push_str(leaf.region_index.to_string().as_str());
    output.push_str("\n    shard: ");
    output.push_str(leaf.shard.to_string().as_str());
    output.push('\n');
    append_comment_variant_suffix(&mut output, variant);
    output
}

fn build_toml_document(leaf: LeafSpec<'_>, variant: usize, ignored: bool) -> String {
    let mut output = String::with_capacity(1_280);
    output.push_str("[tenant]\n");
    output.push_str("id = \"tenant-");
    push_two_digits(&mut output, leaf.tenant as u8);
    output.push_str("\"\n\n");
    output.push_str("[tenant.service]\n");
    output.push_str("name = \"");
    output.push_str(leaf.service_name);
    output.push_str("\"\nregion = \"");
    output.push_str(leaf.region);
    output.push_str("\"\n\n");
    output.push_str("[tenant.service.metadata.owner]\n");
    output.push_str("team = \"platform\"\nrotation = \"");
    output.push_str(if ignored { "shadow" } else { "primary" });
    output.push_str("\"\n\n");
    output.push_str("[[tenant.service.summary.alerts]]\n");
    output.push_str("name = \"latency\"\nstatus = \"ok\"\n\n");
    output.push_str("[[tenant.service.summary.alerts]]\n");
    output.push_str("name = \"replication\"\nstatus = \"failed\"\n\n");
    output.push_str("[deployment.layers]\n");
    output.push_str("batch = ");
    output.push_str(leaf.region_index.to_string().as_str());
    output.push_str("\nshard = ");
    output.push_str(leaf.shard.to_string().as_str());
    output.push('\n');
    append_comment_variant_suffix(&mut output, variant);
    output
}

fn append_json_variant_suffix(output: &mut String, variant: usize) {
    output.push('\n');
    for shift in 0..12 {
        if (variant >> (shift % 4)) & 1 == 0 {
            output.push(' ');
        } else {
            output.push('\t');
        }
    }
    output.push('\n');
}

fn append_comment_variant_suffix(output: &mut String, variant: usize) {
    output.push_str("# variant ");
    push_two_digits(output, variant as u8);
    output.push_str(" sentinel padding block\n");
}

fn push_two_digits(output: &mut String, value: u8) {
    output.push(char::from(b'0' + ((value / 10) % 10)));
    output.push(char::from(b'0' + (value % 10)));
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(4))
        .sample_size(20)
}

criterion_group!(
    name = benches;
    config = criterion_config();
    targets = benchmark_exact_traversal,
        benchmark_approximate_traversal,
        benchmark_structured_json_traversal,
        benchmark_structured_yaml_traversal,
        benchmark_structured_toml_traversal
);
criterion_main!(benches);
