use std::hint::black_box;
use std::time::Duration;

use byteseek::{CaseMode, SearchConfig, Searcher, StructuredMode};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

#[path = "support/variants.rs"]
mod variants;
use variants::bench_independent_variants;

const BENCH_VARIANT_COUNT: usize = 64;

#[derive(Clone, Copy)]
enum StructuredFormat {
    Json,
    Yaml,
    Toml,
}

fn build_minified_json(events: usize) -> String {
    let mut json = String::with_capacity(events * 176);
    json.push_str("{\"events\":[");

    for index in 0..events {
        if index != 0 {
            json.push(',');
        }

        let severity = match index % 7 {
            0 => "critical",
            1 => "warning",
            2 => "info",
            3 => "debug",
            4 => "warning",
            5 => "info",
            _ => "critical",
        };
        let status = if index % 19 == 0 { "failed" } else { "ok" };
        let latency_ms = 100 + (index % 900);

        json.push_str("{\"id\":");
        json.push_str(index.to_string().as_str());
        json.push_str(",\"timestamp\":\"2026-03-18T10:");
        push_two_digits(&mut json, (index % 60) as u8);
        json.push_str(":00Z\",\"latency_ms\":");
        json.push_str(latency_ms.to_string().as_str());
        json.push_str(",\"service\":{\"name\":\"svc-");
        push_three_digits(&mut json, (index % 1000) as u16);
        json.push_str("\",\"region\":\"");
        json.push_str(if index % 2 == 0 { "us-west" } else { "us-east" });
        json.push_str("\"},\"payload\":{\"user\":{\"name\":\"user-");
        push_four_digits(&mut json, (index % 10_000) as u16);
        json.push_str("\",\"tier\":\"");
        json.push_str(if index % 5 == 0 { "gold" } else { "standard" });
        json.push_str("\"},\"severity\":\"");
        json.push_str(severity);
        json.push_str("\",\"status\":\"");
        json.push_str(status);
        json.push_str("\"}}");
    }

    json.push_str("]}");
    json
}

fn build_pretty_json(events: usize) -> String {
    let mut json = String::with_capacity(events * 240);
    json.push_str("{\n  \"events\": [\n");

    for index in 0..events {
        if index != 0 {
            json.push_str(",\n");
        }

        let severity = match index % 7 {
            0 => "critical",
            1 => "warning",
            2 => "info",
            3 => "debug",
            4 => "warning",
            5 => "info",
            _ => "critical",
        };
        let status = if index % 19 == 0 { "failed" } else { "ok" };
        let latency_ms = 100 + (index % 900);

        json.push_str("    {\n");
        json.push_str("      \"id\": ");
        json.push_str(index.to_string().as_str());
        json.push_str(",\n");
        json.push_str("      \"timestamp\": \"2026-03-18T10:");
        push_two_digits(&mut json, (index % 60) as u8);
        json.push_str(":00Z\",\n");
        json.push_str("      \"latency_ms\": ");
        json.push_str(latency_ms.to_string().as_str());
        json.push_str(",\n");
        json.push_str("      \"service\": {\n");
        json.push_str("        \"name\": \"svc-");
        push_three_digits(&mut json, (index % 1000) as u16);
        json.push_str("\",\n");
        json.push_str("        \"region\": \"");
        json.push_str(if index % 2 == 0 { "us-west" } else { "us-east" });
        json.push_str("\"\n");
        json.push_str("      },\n");
        json.push_str("      \"payload\": {\n");
        json.push_str("        \"user\": {\n");
        json.push_str("          \"name\": \"user-");
        push_four_digits(&mut json, (index % 10_000) as u16);
        json.push_str("\",\n");
        json.push_str("          \"tier\": \"");
        json.push_str(if index % 5 == 0 { "gold" } else { "standard" });
        json.push_str("\"\n");
        json.push_str("        },\n");
        json.push_str("        \"severity\": \"");
        json.push_str(severity);
        json.push_str("\",\n");
        json.push_str("        \"status\": \"");
        json.push_str(status);
        json.push_str("\"\n");
        json.push_str("      }\n");
        json.push_str("    }");
    }

    json.push_str("\n  ]\n}\n");
    json
}

fn build_yaml(events: usize) -> String {
    let mut yaml = String::with_capacity(events * 240);
    yaml.push_str("events:\n");

    for index in 0..events {
        let severity = match index % 7 {
            0 => "critical",
            1 => "warning",
            2 => "info",
            3 => "debug",
            4 => "warning",
            5 => "info",
            _ => "critical",
        };
        let status = if index % 19 == 0 { "failed" } else { "ok" };
        let latency_ms = 100 + (index % 900);

        yaml.push_str("  - id: ");
        yaml.push_str(index.to_string().as_str());
        yaml.push_str("\n    timestamp: 2026-03-18T10:");
        push_two_digits(&mut yaml, (index % 60) as u8);
        yaml.push_str(":00Z\n    latency_ms: ");
        yaml.push_str(latency_ms.to_string().as_str());
        yaml.push_str("\n    service:\n      name: svc-");
        push_three_digits(&mut yaml, (index % 1000) as u16);
        yaml.push_str("\n      region: ");
        yaml.push_str(if index % 2 == 0 { "us-west" } else { "us-east" });
        yaml.push_str("\n    payload:\n      user:\n        name: user-");
        push_four_digits(&mut yaml, (index % 10_000) as u16);
        yaml.push_str("\n        tier: ");
        yaml.push_str(if index % 5 == 0 { "gold" } else { "standard" });
        yaml.push_str("\n      severity: ");
        yaml.push_str(severity);
        yaml.push_str("\n      status: ");
        yaml.push_str(status);
        yaml.push('\n');
    }

    yaml
}

fn build_toml(events: usize) -> String {
    let mut toml = String::with_capacity(events * 232);

    for index in 0..events {
        let severity = match index % 7 {
            0 => "critical",
            1 => "warning",
            2 => "info",
            3 => "debug",
            4 => "warning",
            5 => "info",
            _ => "critical",
        };
        let status = if index % 19 == 0 { "failed" } else { "ok" };
        let latency_ms = 100 + (index % 900);

        toml.push_str("[[events]]\n");
        toml.push_str("id = ");
        toml.push_str(index.to_string().as_str());
        toml.push_str("\ntimestamp = \"2026-03-18T10:");
        push_two_digits(&mut toml, (index % 60) as u8);
        toml.push_str(":00Z\"\nlatency_ms = ");
        toml.push_str(latency_ms.to_string().as_str());
        toml.push_str("\nservice.name = \"svc-");
        push_three_digits(&mut toml, (index % 1000) as u16);
        toml.push_str("\"\nservice.region = \"");
        toml.push_str(if index % 2 == 0 { "us-west" } else { "us-east" });
        toml.push_str("\"\npayload.user.name = \"user-");
        push_four_digits(&mut toml, (index % 10_000) as u16);
        toml.push_str("\"\npayload.user.tier = \"");
        toml.push_str(if index % 5 == 0 { "gold" } else { "standard" });
        toml.push_str("\"\npayload.severity = \"");
        toml.push_str(severity);
        toml.push_str("\"\npayload.status = \"");
        toml.push_str(status);
        toml.push_str("\"\n\n");
    }

    toml
}

fn push_three_digits(output: &mut String, value: u16) {
    output.push(char::from(b'0' + ((value / 100) % 10) as u8));
    output.push(char::from(b'0' + ((value / 10) % 10) as u8));
    output.push(char::from(b'0' + (value % 10) as u8));
}

fn push_two_digits(output: &mut String, value: u8) {
    output.push(char::from(b'0' + ((value / 10) % 10)));
    output.push(char::from(b'0' + (value % 10)));
}

fn push_four_digits(output: &mut String, value: u16) {
    output.push(char::from(b'0' + ((value / 1000) % 10) as u8));
    output.push(char::from(b'0' + ((value / 100) % 10) as u8));
    output.push(char::from(b'0' + ((value / 10) % 10) as u8));
    output.push(char::from(b'0' + (value % 10) as u8));
}

fn build_benchmark_variants(base: String, format: StructuredFormat) -> Vec<String> {
    let mut variants = Vec::with_capacity(BENCH_VARIANT_COUNT);
    for index in 0..BENCH_VARIANT_COUNT {
        let mut variant = base.clone();
        append_variant_suffix(&mut variant, format, index);
        variants.push(variant);
    }
    variants
}

fn append_variant_suffix(output: &mut String, format: StructuredFormat, index: usize) {
    match format {
        StructuredFormat::Json => {
            output.push('\n');
            for _ in 0..=index {
                output.push(' ');
            }
        }
        StructuredFormat::Yaml | StructuredFormat::Toml => {
            output.push_str("\n# benchmark variant ");
            output.push_str(index.to_string().as_str());
            output.push('\n');
        }
    }
}

fn benchmark_structured_find(c: &mut Criterion) {
    let mut group = c.benchmark_group("structured_json_find");

    for events in [1_000_usize, 10_000_usize] {
        let variants =
            build_benchmark_variants(build_minified_json(events), StructuredFormat::Json);
        group.throughput(Throughput::Bytes(variants[0].len() as u64));

        let path_searcher = Searcher::new(
            ["$.events[777].payload.user.name"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Json))
                .build(),
        )
        .expect("structured path searcher should build");
        group.bench_with_input(
            BenchmarkId::new("path", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(path_searcher.find(text));
                });
            },
        );

        let value_searcher = Searcher::new(
            ["critical"],
            SearchConfig::builder()
                .case_mode(CaseMode::AsciiInsensitive)
                .structured_mode(Some(StructuredMode::Json))
                .build(),
        )
        .expect("structured value searcher should build");
        group.bench_with_input(
            BenchmarkId::new("value", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(value_searcher.find(text));
                });
            },
        );

        let path_value_searcher = Searcher::new(
            ["$.events[38].payload.status=failed"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Json))
                .build(),
        )
        .expect("structured path-value searcher should build");
        group.bench_with_input(
            BenchmarkId::new("path_value", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(path_value_searcher.find(text));
                });
            },
        );
    }

    group.finish();
}

fn benchmark_structured_path_limits(c: &mut Criterion) {
    let mut group = c.benchmark_group("structured_path_limits");
    let variants = build_benchmark_variants(build_minified_json(10_000), StructuredFormat::Json);
    group.throughput(Throughput::Bytes(variants[0].len() as u64));

    let patterns = (0..1_000)
        .map(|index| format!("$.events[{index}].payload.status"))
        .collect::<Vec<_>>();
    let unlimited = Searcher::new(
        patterns.iter().map(String::as_str),
        SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .build(),
    )
    .expect("unlimited structured searcher should build");
    let limited = Searcher::new(
        patterns.iter().map(String::as_str),
        SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .max_matches(Some(1))
            .build(),
    )
    .expect("limited structured searcher should build");

    group.bench_function("path_limit_1", |bench| {
        bench_independent_variants(bench, &variants, |text| {
            black_box(limited.find(text));
        });
    });
    group.bench_function("path_unlimited_baseline", |bench| {
        bench_independent_variants(bench, &variants, |text| {
            black_box(unlimited.find(text));
        });
    });

    let unlimited_value = Searcher::new(
        ["critical"],
        SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .build(),
    )
    .expect("unlimited structured value searcher should build");
    let limited_value = Searcher::new(
        ["critical"],
        SearchConfig::builder()
            .structured_mode(Some(StructuredMode::Json))
            .max_matches(Some(1))
            .build(),
    )
    .expect("limited structured value searcher should build");

    group.bench_function("value_limit_1", |bench| {
        bench_independent_variants(bench, &variants, |text| {
            black_box(limited_value.find(text));
        });
    });
    group.bench_function("value_unlimited_baseline", |bench| {
        bench_independent_variants(bench, &variants, |text| {
            black_box(unlimited_value.find(text));
        });
    });

    group.finish();
}

fn benchmark_structured_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("structured_json_lines");

    for events in [1_000_usize, 10_000_usize] {
        let variants = build_benchmark_variants(build_pretty_json(events), StructuredFormat::Json);
        group.throughput(Throughput::Bytes(variants[0].len() as u64));

        let searcher = Searcher::new(
            ["$.events[38].payload.status"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Json))
                .build(),
        )
        .expect("structured line searcher should build");

        group.bench_with_input(
            BenchmarkId::new("find_lines", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(searcher.find_lines(text));
                });
            },
        );
    }

    group.finish();
}

fn benchmark_structured_yaml_find(c: &mut Criterion) {
    let mut group = c.benchmark_group("structured_yaml_find");

    for events in [1_000_usize, 10_000_usize] {
        let variants = build_benchmark_variants(build_yaml(events), StructuredFormat::Yaml);
        group.throughput(Throughput::Bytes(variants[0].len() as u64));

        let path_searcher = Searcher::new(
            ["$.events[777].payload.user.name"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Yaml))
                .build(),
        )
        .expect("structured path searcher should build");
        group.bench_with_input(
            BenchmarkId::new("path", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(path_searcher.find(text));
                });
            },
        );

        let value_searcher = Searcher::new(
            ["critical"],
            SearchConfig::builder()
                .case_mode(CaseMode::AsciiInsensitive)
                .structured_mode(Some(StructuredMode::Yaml))
                .build(),
        )
        .expect("structured value searcher should build");
        group.bench_with_input(
            BenchmarkId::new("value", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(value_searcher.find(text));
                });
            },
        );

        let path_value_searcher = Searcher::new(
            ["$.events[38].payload.status=failed"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Yaml))
                .build(),
        )
        .expect("structured path-value searcher should build");
        group.bench_with_input(
            BenchmarkId::new("path_value", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(path_value_searcher.find(text));
                });
            },
        );
    }

    group.finish();
}

fn benchmark_structured_yaml_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("structured_yaml_lines");

    for events in [1_000_usize, 10_000_usize] {
        let variants = build_benchmark_variants(build_yaml(events), StructuredFormat::Yaml);
        group.throughput(Throughput::Bytes(variants[0].len() as u64));

        let searcher = Searcher::new(
            ["$.events[38].payload.status"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Yaml))
                .build(),
        )
        .expect("structured line searcher should build");

        group.bench_with_input(
            BenchmarkId::new("find_lines", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(searcher.find_lines(text));
                });
            },
        );
    }

    group.finish();
}

fn benchmark_structured_toml_find(c: &mut Criterion) {
    let mut group = c.benchmark_group("structured_toml_find");

    for events in [1_000_usize, 10_000_usize] {
        let variants = build_benchmark_variants(build_toml(events), StructuredFormat::Toml);
        group.throughput(Throughput::Bytes(variants[0].len() as u64));

        let path_searcher = Searcher::new(
            ["$.events[777].payload.user.name"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Toml))
                .build(),
        )
        .expect("structured path searcher should build");
        group.bench_with_input(
            BenchmarkId::new("path", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(path_searcher.find(text));
                });
            },
        );

        let value_searcher = Searcher::new(
            ["critical"],
            SearchConfig::builder()
                .case_mode(CaseMode::AsciiInsensitive)
                .structured_mode(Some(StructuredMode::Toml))
                .build(),
        )
        .expect("structured value searcher should build");
        group.bench_with_input(
            BenchmarkId::new("value", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(value_searcher.find(text));
                });
            },
        );

        let path_value_searcher = Searcher::new(
            ["$.events[38].payload.status=failed"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Toml))
                .build(),
        )
        .expect("structured path-value searcher should build");
        group.bench_with_input(
            BenchmarkId::new("path_value", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(path_value_searcher.find(text));
                });
            },
        );
    }

    group.finish();
}

fn benchmark_structured_toml_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("structured_toml_lines");

    for events in [1_000_usize, 10_000_usize] {
        let variants = build_benchmark_variants(build_toml(events), StructuredFormat::Toml);
        group.throughput(Throughput::Bytes(variants[0].len() as u64));

        let searcher = Searcher::new(
            ["$.events[38].payload.status"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Toml))
                .build(),
        )
        .expect("structured line searcher should build");

        group.bench_with_input(
            BenchmarkId::new("find_lines", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(searcher.find_lines(text));
                });
            },
        );
    }

    group.finish();
}

fn benchmark_structured_json_predicates(c: &mut Criterion) {
    let mut group = c.benchmark_group("structured_json_predicates");

    for events in [1_000_usize, 10_000_usize] {
        let variants =
            build_benchmark_variants(build_minified_json(events), StructuredFormat::Json);
        group.throughput(Throughput::Bytes(variants[0].len() as u64));

        let numeric_searcher = Searcher::new(
            ["$.latency_ms > 500"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Json))
                .build(),
        )
        .expect("structured numeric predicate searcher should build");
        group.bench_with_input(
            BenchmarkId::new("numeric", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(numeric_searcher.find(text));
                });
            },
        );

        let datetime_searcher = Searcher::new(
            ["$.timestamp between \"2026-03-18T10:10:00Z\" and \"2026-03-18T10:40:00Z\""],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Json))
                .build(),
        )
        .expect("structured datetime predicate searcher should build");
        group.bench_with_input(
            BenchmarkId::new("datetime", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(datetime_searcher.find(text));
                });
            },
        );

        let cross_field_searcher = Searcher::new(
            ["$.service.name=\"svc-038\" AND $.payload.status=failed"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Json))
                .build(),
        )
        .expect("structured cross-field predicate searcher should build");
        group.bench_with_input(
            BenchmarkId::new("cross_field", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(cross_field_searcher.find(text));
                });
            },
        );
    }

    group.finish();
}

fn benchmark_structured_yaml_predicates(c: &mut Criterion) {
    let mut group = c.benchmark_group("structured_yaml_predicates");

    for events in [1_000_usize, 10_000_usize] {
        let variants = build_benchmark_variants(build_yaml(events), StructuredFormat::Yaml);
        group.throughput(Throughput::Bytes(variants[0].len() as u64));

        let numeric_searcher = Searcher::new(
            ["$.latency_ms > 500"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Yaml))
                .build(),
        )
        .expect("structured numeric predicate searcher should build");
        group.bench_with_input(
            BenchmarkId::new("numeric", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(numeric_searcher.find(text));
                });
            },
        );

        let datetime_searcher = Searcher::new(
            ["$.timestamp between 2026-03-18T10:10:00Z and 2026-03-18T10:40:00Z"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Yaml))
                .build(),
        )
        .expect("structured datetime predicate searcher should build");
        group.bench_with_input(
            BenchmarkId::new("datetime", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(datetime_searcher.find(text));
                });
            },
        );

        let cross_field_searcher = Searcher::new(
            ["$.service.name=svc-038 AND $.payload.status=failed"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Yaml))
                .build(),
        )
        .expect("structured cross-field predicate searcher should build");
        group.bench_with_input(
            BenchmarkId::new("cross_field", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(cross_field_searcher.find(text));
                });
            },
        );
    }

    group.finish();
}

fn benchmark_structured_toml_predicates(c: &mut Criterion) {
    let mut group = c.benchmark_group("structured_toml_predicates");

    for events in [1_000_usize, 10_000_usize] {
        let variants = build_benchmark_variants(build_toml(events), StructuredFormat::Toml);
        group.throughput(Throughput::Bytes(variants[0].len() as u64));

        let numeric_searcher = Searcher::new(
            ["$.latency_ms > 500"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Toml))
                .build(),
        )
        .expect("structured numeric predicate searcher should build");
        group.bench_with_input(
            BenchmarkId::new("numeric", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(numeric_searcher.find(text));
                });
            },
        );

        let datetime_searcher = Searcher::new(
            ["$.timestamp between \"2026-03-18T10:10:00Z\" and \"2026-03-18T10:40:00Z\""],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Toml))
                .build(),
        )
        .expect("structured datetime predicate searcher should build");
        group.bench_with_input(
            BenchmarkId::new("datetime", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(datetime_searcher.find(text));
                });
            },
        );

        let cross_field_searcher = Searcher::new(
            ["$.service.name=\"svc-038\" AND $.payload.status=failed"],
            SearchConfig::builder()
                .structured_mode(Some(StructuredMode::Toml))
                .build(),
        )
        .expect("structured cross-field predicate searcher should build");
        group.bench_with_input(
            BenchmarkId::new("cross_field", events),
            &variants,
            |bench, haystacks| {
                bench_independent_variants(bench, haystacks, |text| {
                    black_box(cross_field_searcher.find(text));
                });
            },
        );
    }

    group.finish();
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
    targets = benchmark_structured_find,
        benchmark_structured_path_limits,
        benchmark_structured_lines,
        benchmark_structured_yaml_find,
        benchmark_structured_yaml_lines,
        benchmark_structured_toml_find,
        benchmark_structured_toml_lines,
        benchmark_structured_json_predicates,
        benchmark_structured_yaml_predicates,
        benchmark_structured_toml_predicates
);
criterion_main!(benches);
