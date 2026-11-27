use std::hint::black_box;
use std::time::Duration;

use byteseek::{CaseMode, SearchConfig, Searcher};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

fn build_log_corpus(lines: usize) -> String {
    let mut corpus = String::with_capacity(lines * 96);
    for index in 0..lines {
        let level = match index % 8 {
            0 => "INFO",
            1 => "DEBUG",
            2 => "WARN",
            3 => "ERROR",
            4 => "TRACE",
            5 => "INFO",
            6 => "WARN",
            _ => "ERROR",
        };

        corpus.push_str("2026-01-01T00:");
        let minute = (index % 60) as u8;
        let second = ((index * 7) % 60) as u8;
        push_two_digits(&mut corpus, minute);
        corpus.push(':');
        push_two_digits(&mut corpus, second);
        corpus.push(' ');
        corpus.push_str(level);
        corpus.push_str(" service=auth request_id=");
        corpus.push_str(&index.to_string());
        corpus.push_str(" status=");
        corpus.push_str(if index % 17 == 0 { "500" } else { "200" });
        corpus.push_str(" message=handled user event\n");
    }
    corpus
}

fn build_source_corpus(repetitions: usize) -> String {
    let snippet = r#"
pub fn parse_record(input: &str) -> Result<Record, ParseError> {
    let mut fields = input.split(',');
    let id = fields.next().ok_or(ParseError::MissingField("id"))?;
    let category = fields.next().ok_or(ParseError::MissingField("category"))?;
    let payload = fields.next().ok_or(ParseError::MissingField("payload"))?;
    Ok(Record::new(id, category, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_record() {
        let result = parse_record("42,alpha,payload");
        assert!(result.is_ok());
    }
}
"#;

    snippet.repeat(repetitions)
}

fn build_dna_corpus(length: usize) -> String {
    let mut seed = 0x1234_5678_9abc_def0_u64;
    let mut data = String::with_capacity(length);

    for _ in 0..length {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let next = ((seed >> 32) & 0b11) as u8;
        let byte = match next {
            0 => 'A',
            1 => 'C',
            2 => 'G',
            _ => 'T',
        };
        data.push(byte);
    }

    data
}

fn build_overlap_corpus(repetitions: usize) -> String {
    "bananana".repeat(repetitions)
}

fn push_two_digits(output: &mut String, value: u8) {
    output.push(char::from(b'0' + (value / 10)));
    output.push(char::from(b'0' + (value % 10)));
}

fn benchmark_single_pattern(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_pattern");

    for lines in [2_000_usize, 20_000_usize] {
        let haystack = build_log_corpus(lines);
        group.throughput(Throughput::Bytes(haystack.len() as u64));

        let sensitive = Searcher::new(["ERROR"], SearchConfig::default())
            .expect("single pattern should compile");
        group.bench_with_input(
            BenchmarkId::new("sensitive_logs", lines),
            &haystack,
            |bench, text| {
                bench.iter(|| black_box(sensitive.find(black_box(text))));
            },
        );

        let insensitive_config = SearchConfig::builder()
            .case_mode(CaseMode::AsciiInsensitive)
            .build();
        let insensitive =
            Searcher::new(["warn"], insensitive_config).expect("single pattern should compile");

        group.bench_with_input(
            BenchmarkId::new("ascii_insensitive_logs", lines),
            &haystack,
            |bench, text| {
                bench.iter(|| black_box(insensitive.find(black_box(text))));
            },
        );
    }

    group.finish();
}

fn benchmark_multi_pattern(c: &mut Criterion) {
    let mut group = c.benchmark_group("multi_pattern");

    let corpus = build_source_corpus(2_000);
    group.throughput(Throughput::Bytes(corpus.len() as u64));

    let patterns = [
        "parse_record",
        "MissingField",
        "result.is_ok",
        "Record::new",
        "payload",
        "assert",
    ];

    let searcher = Searcher::new(patterns, SearchConfig::default())
        .expect("multi-pattern searcher should compile");

    group.bench_function("source_code_tokens", |bench| {
        bench.iter(|| black_box(searcher.find(black_box(corpus.as_str()))));
    });

    group.finish();
}

fn benchmark_dna(c: &mut Criterion) {
    let mut group = c.benchmark_group("dna_like");

    for length in [250_000_usize, 1_000_000_usize] {
        let corpus = build_dna_corpus(length);
        group.throughput(Throughput::Bytes(corpus.len() as u64));

        let searcher = Searcher::new(["ACGTAC"], SearchConfig::default())
            .expect("single pattern should compile");

        group.bench_with_input(
            BenchmarkId::new("single_pattern", length),
            &corpus,
            |bench, text| {
                bench.iter(|| black_box(searcher.find(black_box(text))));
            },
        );

        let multi = Searcher::new(
            ["ACGT", "GTAC", "TTTTT", "CCCC", "GGGG", "AAAA"],
            SearchConfig::default(),
        )
        .expect("multi pattern should compile");

        group.bench_with_input(
            BenchmarkId::new("multi_pattern", length),
            &corpus,
            |bench, text| {
                bench.iter(|| black_box(multi.find(black_box(text))));
            },
        );
    }

    group.finish();
}

fn benchmark_overlap_and_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("overlap_and_lines");

    let overlap_text = build_overlap_corpus(100_000);
    group.throughput(Throughput::Bytes(overlap_text.len() as u64));

    let overlap_config = SearchConfig::builder().overlapping(true).build();
    let overlap_searcher =
        Searcher::new(["ana"], overlap_config).expect("overlap pattern should compile");

    group.bench_function("overlap_heavy", |bench| {
        bench.iter(|| black_box(overlap_searcher.find(black_box(overlap_text.as_str()))));
    });

    let line_text = build_log_corpus(120_000);
    group.throughput(Throughput::Bytes(line_text.len() as u64));

    let config = SearchConfig::builder()
        .case_mode(CaseMode::AsciiInsensitive)
        .build();
    let line_searcher = Searcher::new(["error", "warn", "status=500"], config)
        .expect("line searcher should compile");

    group.bench_function("line_auto_dispatch", |bench| {
        bench.iter(|| black_box(line_searcher.find_lines(black_box(line_text.as_str()))));
    });

    group.bench_function("line_parallel", |bench| {
        bench.iter(|| black_box(line_searcher.find_lines_parallel(black_box(line_text.as_str()))));
    });

    group.finish();
}

fn benchmark_count_without_materialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("count_without_materialization");
    let corpus = "needle warning info ".repeat(2_000);
    group.throughput(Throughput::Bytes(corpus.len() as u64));

    let single = Searcher::new(["needle"], SearchConfig::default())
        .expect("single-pattern searcher should compile");
    group.bench_function("single_direct", |bench| {
        bench.iter(|| black_box(single.count(black_box(corpus.as_str()))));
    });
    group.bench_function("single_materialized_baseline", |bench| {
        bench.iter(|| black_box(single.find(black_box(corpus.as_str())).len()));
    });

    let multi = Searcher::new(["needle", "warning", "info"], SearchConfig::default())
        .expect("multi-pattern searcher should compile");
    group.bench_function("multi_direct", |bench| {
        bench.iter(|| black_box(multi.count(black_box(corpus.as_str()))));
    });
    group.bench_function("multi_materialized_baseline", |bench| {
        bench.iter(|| black_box(multi.find(black_box(corpus.as_str())).len()));
    });

    group.finish();
}

fn benchmark_boolean_queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("boolean_queries");

    for lines in [20_000_usize, 120_000_usize] {
        let haystack = build_log_corpus(lines);
        group.throughput(Throughput::Bytes(haystack.len() as u64));

        let baseline_and = Searcher::new(["ERROR", "status=500"], SearchConfig::default())
            .expect("baseline term-union searcher should compile");
        group.bench_with_input(
            BenchmarkId::new("baseline_and_union_line_count", lines),
            &haystack,
            |bench, text| {
                bench.iter(|| black_box(baseline_and.count_matching_lines(black_box(text))));
            },
        );

        let boolean_config = SearchConfig::builder().boolean_queries(true).build();
        let boolean_and = Searcher::new(["ERROR AND status=500"], boolean_config)
            .expect("boolean searcher should compile");
        group.bench_with_input(
            BenchmarkId::new("boolean_and_line_count", lines),
            &haystack,
            |bench, text| {
                bench.iter(|| black_box(boolean_and.count_matching_lines(black_box(text))));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("boolean_and_find_lines", lines),
            &haystack,
            |bench, text| {
                bench.iter(|| black_box(boolean_and.find_lines(black_box(text))));
            },
        );

        let baseline_complex = Searcher::new(
            [
                "ERROR",
                "status=500",
                "WARN",
                "status=200",
                "service=auth",
                "TRACE",
            ],
            SearchConfig::default(),
        )
        .expect("complex baseline searcher should compile");
        group.bench_with_input(
            BenchmarkId::new("baseline_complex_union_line_count", lines),
            &haystack,
            |bench, text| {
                bench.iter(|| black_box(baseline_complex.count_matching_lines(black_box(text))));
            },
        );

        let complex = Searcher::new(
            ["((ERROR AND status=500) OR (WARN AND status=200)) AND service=auth AND NOT TRACE"],
            boolean_config,
        )
        .expect("complex boolean searcher should compile");
        group.bench_with_input(
            BenchmarkId::new("boolean_complex_line_count", lines),
            &haystack,
            |bench, text| {
                bench.iter(|| black_box(complex.count_matching_lines(black_box(text))));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("boolean_complex_find_lines", lines),
            &haystack,
            |bench, text| {
                bench.iter(|| black_box(complex.find_lines(black_box(text))));
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
    targets = benchmark_single_pattern,
        benchmark_multi_pattern,
        benchmark_dna,
        benchmark_overlap_and_lines,
        benchmark_count_without_materialization,
        benchmark_boolean_queries
);
criterion_main!(benches);
