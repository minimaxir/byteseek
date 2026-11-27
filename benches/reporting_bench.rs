use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Duration;

use byteseek::{CaseMode, SearchConfig, Searcher, StructuredMode};
use criterion::{Bencher, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tempfile::TempDir;

const ROOT_VARIANT_COUNT: usize = 8;
const TEXT_FILE_COUNT: usize = 36;
const STRUCTURED_FILE_COUNT: usize = 24;

struct TraversalFixture {
    _roots: Vec<TempDir>,
    paths: Vec<PathBuf>,
    visible_files: usize,
    visible_bytes: u64,
}

fn bench_independent_roots(
    bench: &mut Bencher<'_>,
    fixture: &TraversalFixture,
    mut run: impl FnMut(&Path),
) {
    bench.iter(|| {
        for path in &fixture.paths {
            run(black_box(path.as_path()));
        }
    });
}

fn benchmark_exact_reporting_generation(c: &mut Criterion) {
    let fixture = build_text_fixture();
    let searcher = Searcher::new(
        ["ERROR", "timeout", "status=500"],
        SearchConfig::builder()
            .case_mode(CaseMode::AsciiInsensitive)
            .build(),
    )
    .expect("exact reporting benchmark searcher should build");

    let mut group = c.benchmark_group("reporting_exact_generation");
    group.throughput(Throughput::Bytes(fixture.visible_bytes));

    group.bench_with_input(
        BenchmarkId::new("search_only", fixture.visible_files * fixture.paths.len()),
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

    group.bench_with_input(
        BenchmarkId::new(
            "search_plus_report",
            fixture.visible_files * fixture.paths.len(),
        ),
        &fixture,
        |bench, current_fixture| {
            bench_independent_roots(bench, current_fixture, |root| {
                black_box(
                    searcher
                        .report_path(root)
                        .expect("exact reporting benchmark should succeed"),
                );
            });
        },
    );

    group.finish();
}

fn benchmark_structured_reporting_generation(c: &mut Criterion) {
    let fixture = build_structured_fixture();
    let searcher = Searcher::new(
        [
            "$.tenant.service.name",
            "$.tenant.service.status",
            "$.tenant.service.status=failed",
            "payments",
        ],
        SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .build(),
    )
    .expect("structured reporting benchmark searcher should build");

    let mut group = c.benchmark_group("reporting_structured_generation");
    group.throughput(Throughput::Bytes(fixture.visible_bytes));

    group.bench_with_input(
        BenchmarkId::new("search_only", fixture.visible_files * fixture.paths.len()),
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

    group.bench_with_input(
        BenchmarkId::new(
            "search_plus_report",
            fixture.visible_files * fixture.paths.len(),
        ),
        &fixture,
        |bench, current_fixture| {
            bench_independent_roots(bench, current_fixture, |root| {
                black_box(
                    searcher
                        .structured_report_path(root)
                        .expect("structured reporting benchmark should succeed"),
                );
            });
        },
    );

    group.finish();
}

fn benchmark_reporting_renderers(c: &mut Criterion) {
    let text_fixture = build_text_fixture();
    let exact_searcher = Searcher::new(
        ["ERROR", "timeout", "status=500"],
        SearchConfig::builder()
            .case_mode(CaseMode::AsciiInsensitive)
            .build(),
    )
    .expect("exact reporting benchmark searcher should build");
    let exact_report = exact_searcher
        .report_path(text_fixture.paths[0].as_path())
        .expect("exact report should build");

    let structured_fixture = build_structured_fixture();
    let structured_searcher = Searcher::new(
        [
            "$.tenant.service.name",
            "$.tenant.service.status",
            "$.tenant.service.status=failed",
            "payments",
        ],
        SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .build(),
    )
    .expect("structured reporting benchmark searcher should build");
    let structured_report = structured_searcher
        .structured_report_path(structured_fixture.paths[0].as_path())
        .expect("structured report should build");

    let mut group = c.benchmark_group("reporting_renderers");

    group.bench_function("exact_summary", |bench| {
        bench.iter(|| black_box(exact_report.render_summary()));
    });
    group.bench_function("exact_top_files_table", |bench| {
        bench.iter(|| black_box(exact_report.render_top_files_table(10)));
    });
    group.bench_function("exact_directory_density", |bench| {
        bench.iter(|| black_box(exact_report.render_directory_density(10)));
    });
    group.bench_function("exact_pattern_table", |bench| {
        bench.iter(|| black_box(exact_report.render_pattern_frequency_table()));
    });
    group.bench_function("structured_summary", |bench| {
        bench.iter(|| black_box(structured_report.render_summary()));
    });
    group.bench_function("structured_top_files_table", |bench| {
        bench.iter(|| black_box(structured_report.render_top_files_table(10)));
    });
    group.bench_function("structured_directory_density", |bench| {
        bench.iter(|| black_box(structured_report.render_directory_density(10)));
    });
    group.bench_function("structured_pattern_table", |bench| {
        bench.iter(|| black_box(structured_report.render_pattern_frequency_table()));
    });
    group.bench_function("structured_path_table", |bench| {
        bench.iter(|| black_box(structured_report.render_path_frequency_table(10)));
    });
    group.bench_function("structured_value_table", |bench| {
        bench.iter(|| {
            black_box(
                structured_report
                    .render_value_frequency_table("$.tenant.service.status", 10)
                    .expect("status path should be present"),
            )
        });
    });

    group.finish();
}

fn build_text_fixture() -> TraversalFixture {
    let mut roots = Vec::with_capacity(ROOT_VARIANT_COUNT);
    let mut paths = Vec::with_capacity(ROOT_VARIANT_COUNT);
    let mut visible_bytes = 0_u64;

    for variant in 0..ROOT_VARIANT_COUNT {
        let root = TempDir::new().expect("temporary directory should be created");
        let root_path = root.path().to_path_buf();

        fs::write(root_path.join(".gitignore"), "ignored/\n").expect("gitignore should be written");

        for file_index in 0..TEXT_FILE_COUNT {
            let relative = format!("logs/shard-{}/service-{:02}.log", variant % 3, file_index);
            let visible = build_text_log(file_index, variant, false);
            visible_bytes += visible.len() as u64;
            write_file(root_path.as_path(), relative.as_str(), visible.as_str());

            let ignored_relative = format!("ignored/service-{:02}.log", file_index);
            let ignored = build_text_log(file_index, variant, true);
            write_file(
                root_path.as_path(),
                ignored_relative.as_str(),
                ignored.as_str(),
            );
        }

        paths.push(root_path);
        roots.push(root);
    }

    TraversalFixture {
        _roots: roots,
        paths,
        visible_files: TEXT_FILE_COUNT,
        visible_bytes,
    }
}

fn build_structured_fixture() -> TraversalFixture {
    let mut roots = Vec::with_capacity(ROOT_VARIANT_COUNT);
    let mut paths = Vec::with_capacity(ROOT_VARIANT_COUNT);
    let mut visible_bytes = 0_u64;

    for variant in 0..ROOT_VARIANT_COUNT {
        let root = TempDir::new().expect("temporary directory should be created");
        let root_path = root.path().to_path_buf();

        fs::write(root_path.join(".gitignore"), "ignored/\n").expect("gitignore should be written");

        for file_index in 0..STRUCTURED_FILE_COUNT {
            let relative = format!(
                "events/tenant-{}/service-{:02}.json",
                variant % 4,
                file_index
            );
            let visible = build_structured_json(file_index, variant, false);
            visible_bytes += visible.len() as u64;
            write_file(root_path.as_path(), relative.as_str(), visible.as_str());

            let ignored_relative = format!("ignored/service-{:02}.json", file_index);
            let ignored = build_structured_json(file_index, variant, true);
            write_file(
                root_path.as_path(),
                ignored_relative.as_str(),
                ignored.as_str(),
            );
        }

        paths.push(root_path);
        roots.push(root);
    }

    TraversalFixture {
        _roots: roots,
        paths,
        visible_files: STRUCTURED_FILE_COUNT,
        visible_bytes,
    }
}

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory should be created");
    }
    fs::write(path, contents).expect("fixture file should be written");
}

fn build_text_log(file_index: usize, variant: usize, ignored: bool) -> String {
    let mut log = String::with_capacity(10_240);
    for line_index in 0..240_usize {
        let severity = match (file_index + line_index + variant) % 9 {
            0 => "ERROR",
            1 | 2 => "WARN",
            _ => "INFO",
        };
        let status = if (file_index + line_index + variant).is_multiple_of(13) {
            "500"
        } else {
            "200"
        };
        let service = match (file_index + variant) % 3 {
            0 => "payments",
            1 => "auth",
            _ => "search",
        };
        let timeout = if (line_index + file_index).is_multiple_of(17) {
            " timeout=upstream"
        } else {
            ""
        };

        log.push_str("2026-03-18T12:");
        push_two_digits(&mut log, (line_index % 60) as u8);
        log.push(':');
        push_two_digits(&mut log, ((line_index * 7) % 60) as u8);
        log.push(' ');
        log.push_str(severity);
        log.push_str(" service=");
        log.push_str(service);
        log.push_str(" status=");
        log.push_str(status);
        log.push_str(" request_id=");
        log.push_str(
            (variant * 10_000 + file_index * 512 + line_index)
                .to_string()
                .as_str(),
        );
        log.push_str(timeout);
        if ignored {
            log.push_str(" ignored=true");
        }
        log.push('\n');
    }
    log
}

fn build_structured_json(file_index: usize, variant: usize, ignored: bool) -> String {
    let service = match (file_index + variant) % 3 {
        0 => "payments",
        1 => "auth",
        _ => "search",
    };
    let status = if (file_index + variant).is_multiple_of(5) {
        "failed"
    } else {
        "ok"
    };
    let region = if (file_index + variant).is_multiple_of(2) {
        "us-west"
    } else {
        "eu-central"
    };

    format!(
        concat!(
            "{{",
            "\"tenant\":{{",
            "\"id\":\"tenant-{tenant}\",",
            "\"service\":{{",
            "\"name\":\"{service}\",",
            "\"region\":\"{region}\",",
            "\"status\":\"{status}\",",
            "\"metadata\":{{\"owner\":{{\"team\":\"platform\"}}}},",
            "\"summary\":{{",
            "\"alerts\":[",
            "{{\"kind\":\"latency\",\"status\":\"ok\"}},",
            "{{\"kind\":\"health\",\"status\":\"{status}\"}}",
            "],",
            "\"ignored\":{ignored}",
            "}}",
            "}}",
            "}}",
            "}}"
        ),
        tenant = variant * 100 + file_index,
        service = service,
        region = region,
        status = status,
        ignored = if ignored { "true" } else { "false" },
    )
}

fn push_two_digits(output: &mut String, value: u8) {
    output.push(char::from(b'0' + (value / 10)));
    output.push(char::from(b'0' + (value % 10)));
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(4))
        .sample_size(30)
}

criterion_group!(
    name = benches;
    config = criterion_config();
    targets = benchmark_exact_reporting_generation,
        benchmark_structured_reporting_generation,
        benchmark_reporting_renderers
);
criterion_main!(benches);
