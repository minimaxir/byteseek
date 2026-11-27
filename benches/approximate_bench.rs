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

fn build_overlap_corpus(repetitions: usize) -> String {
    "bananana".repeat(repetitions)
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

fn push_two_digits(output: &mut String, value: u8) {
    output.push(char::from(b'0' + (value / 10)));
    output.push(char::from(b'0' + (value % 10)));
}

fn benchmark_approximate_single_pattern(c: &mut Criterion) {
    let mut group = c.benchmark_group("approximate_single_pattern");

    for lines in [2_000_usize, 20_000_usize] {
        let haystack = build_log_corpus(lines);
        group.throughput(Throughput::Bytes(haystack.len() as u64));

        let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
        let searcher = Searcher::new(["EROR"], config).expect("approximate searcher should build");
        group.bench_with_input(
            BenchmarkId::new("sensitive_logs", lines),
            &haystack,
            |bench, text| {
                bench.iter(|| black_box(searcher.find(black_box(text))));
            },
        );

        let insensitive_config = SearchConfig::builder()
            .case_mode(CaseMode::AsciiInsensitive)
            .max_edit_distance(Some(1))
            .build();
        let insensitive =
            Searcher::new(["worn"], insensitive_config).expect("approximate searcher should build");
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

fn benchmark_approximate_multi_pattern(c: &mut Criterion) {
    let mut group = c.benchmark_group("approximate_multi_pattern");

    let corpus = build_source_corpus(2_000);
    group.throughput(Throughput::Bytes(corpus.len() as u64));

    let config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let searcher = Searcher::new(
        [
            "parse_rekord",
            "MissingFeild",
            "result.is_ok",
            "Rekord::new",
            "paylod",
            "asert",
        ],
        config,
    )
    .expect("approximate multi-pattern searcher should build");

    group.bench_function("source_code_tokens", |bench| {
        bench.iter(|| black_box(searcher.find(black_box(corpus.as_str()))));
    });

    group.finish();
}

fn benchmark_approximate_overlap_and_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("approximate_overlap_and_lines");

    let overlap_text = build_overlap_corpus(60_000);
    group.throughput(Throughput::Bytes(overlap_text.len() as u64));

    let overlap_config = SearchConfig::builder()
        .overlapping(true)
        .max_edit_distance(Some(1))
        .build();
    let overlap_searcher =
        Searcher::new(["anna"], overlap_config).expect("approximate overlap searcher should build");

    group.bench_function("overlap_heavy", |bench| {
        bench.iter(|| black_box(overlap_searcher.find(black_box(overlap_text.as_str()))));
    });

    let line_text = build_log_corpus(80_000);
    group.throughput(Throughput::Bytes(line_text.len() as u64));

    let line_config = SearchConfig::builder().max_edit_distance(Some(1)).build();
    let line_searcher =
        Searcher::new(["EROR", "sttus=500"], line_config).expect("line searcher should build");

    group.bench_function("line_find_lines", |bench| {
        bench.iter(|| black_box(line_searcher.find_lines(black_box(line_text.as_str()))));
    });

    group.bench_function("line_parallel_api_sequential_fallback", |bench| {
        bench.iter(|| black_box(line_searcher.find_lines_parallel(black_box(line_text.as_str()))));
    });

    group.finish();
}

fn benchmark_approximate_dna(c: &mut Criterion) {
    let mut group = c.benchmark_group("approximate_dna_like");

    for length in [250_000_usize, 1_000_000_usize] {
        let corpus = build_dna_corpus(length);
        group.throughput(Throughput::Bytes(corpus.len() as u64));

        let single_config = SearchConfig::builder().max_edit_distance(Some(1)).build();
        let single = Searcher::new(["ACGTTC"], single_config)
            .expect("approximate DNA searcher should build");
        group.bench_with_input(
            BenchmarkId::new("single_pattern", length),
            &corpus,
            |bench, text| {
                bench.iter(|| black_box(single.find(black_box(text))));
            },
        );

        let multi_config = SearchConfig::builder().max_edit_distance(Some(1)).build();
        let multi = Searcher::new(
            ["ACGTTC", "GTACCA", "TTTTA", "CCCCG", "GGGGT", "AAAAC"],
            multi_config,
        )
        .expect("approximate DNA multi-pattern searcher should build");
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

fn criterion_config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(4))
        .sample_size(30)
}

criterion_group!(
    name = benches;
    config = criterion_config();
    targets = benchmark_approximate_single_pattern,
        benchmark_approximate_multi_pattern,
        benchmark_approximate_overlap_and_lines,
        benchmark_approximate_dna
);
criterion_main!(benches);
