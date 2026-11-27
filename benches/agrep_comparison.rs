use std::hint::black_box;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

#[path = "support/cli.rs"]
mod cli;

use cli::{byteseek_path, checked_output, write_corpus};

/// Runs `command` and returns its trimmed stdout, panicking if it fails.
fn checked_stdout(command: Command, tool: &str) -> String {
    String::from_utf8(checked_output(command, tool))
        .unwrap_or_else(|error| panic!("{tool} emitted non-UTF-8 stdout: {error}"))
        .trim()
        .to_string()
}

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
        push_two_digits(&mut corpus, (index % 60) as u8);
        corpus.push(':');
        push_two_digits(&mut corpus, ((index * 7) % 60) as u8);
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

fn push_two_digits(output: &mut String, value: u8) {
    output.push(char::from(b'0' + (value / 10)));
    output.push(char::from(b'0' + (value % 10)));
}

fn run_agrep(pattern: &str, path: &Path, max_errors: u32, case_insensitive: bool) -> String {
    let mut command = Command::new("agrep");
    command.arg("-k").arg("-c").arg(format!("-{max_errors}"));
    if case_insensitive {
        command.arg("-i");
    }
    command.arg(pattern).arg(path);
    checked_stdout(command, "TRE agrep")
}

fn run_rust_cli(pattern: &str, path: &Path, max_errors: u32, case_insensitive: bool) -> String {
    let mut command = Command::new(byteseek_path());
    command
        .args([
            "--count",
            "--include-zero",
            "--fixed-strings",
            "--threads",
            "1",
        ])
        .arg("--max-errors")
        .arg(max_errors.to_string());
    if case_insensitive {
        command.arg("--ignore-case");
    }
    command.arg("--regexp").arg(pattern).arg(path);
    checked_stdout(command, "byteseek")
}

fn verify_parity(label: &str, path: &Path, pattern: &str, max_errors: u32, case_insensitive: bool) {
    let rust_output = run_rust_cli(pattern, path, max_errors, case_insensitive);
    let agrep_output = run_agrep(pattern, path, max_errors, case_insensitive);
    assert_eq!(
        rust_output, agrep_output,
        "[{label}] output mismatch: byteseek={rust_output:?}, agrep={agrep_output:?}"
    );
}

fn benchmark_cli_case(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    case_name: &str,
    scale: usize,
    corpus: &str,
    pattern: &str,
    max_errors: u32,
    case_insensitive: bool,
) {
    let file = write_corpus(case_name, corpus);
    let path = file.path();
    verify_parity(case_name, path, pattern, max_errors, case_insensitive);

    group.throughput(Throughput::Bytes(corpus.len() as u64));
    group.bench_function(
        BenchmarkId::new(format!("byteseek/{case_name}"), scale),
        |bench| {
            bench.iter(|| {
                black_box(run_rust_cli(
                    black_box(pattern),
                    black_box(path),
                    max_errors,
                    case_insensitive,
                ))
            });
        },
    );
    group.bench_function(
        BenchmarkId::new(format!("tre_agrep/{case_name}"), scale),
        |bench| {
            bench.iter(|| {
                black_box(run_agrep(
                    black_box(pattern),
                    black_box(path),
                    max_errors,
                    case_insensitive,
                ))
            });
        },
    );
}

fn benchmark_agrep_cli_end_to_end(c: &mut Criterion) {
    let mut group = c.benchmark_group("agrep_cli_end_to_end_single_thread_warm_cache");

    for lines in [2_000_usize, 20_000, 80_000] {
        let corpus = build_log_corpus(lines);
        benchmark_cli_case(
            &mut group,
            "sensitive_logs",
            lines,
            corpus.as_str(),
            "EROR",
            1,
            false,
        );
        benchmark_cli_case(
            &mut group,
            "ascii_insensitive_logs",
            lines,
            corpus.as_str(),
            "worn",
            1,
            true,
        );
    }

    for repetitions in [500_usize, 2_000] {
        let corpus = build_source_corpus(repetitions);
        benchmark_cli_case(
            &mut group,
            "source_code",
            repetitions,
            corpus.as_str(),
            "parse_rekord",
            1,
            false,
        );
    }

    for lines in [20_000_usize, 80_000] {
        let corpus = build_log_corpus(lines);
        benchmark_cli_case(
            &mut group,
            "long_pattern",
            lines,
            corpus.as_str(),
            "sttus=500",
            1,
            false,
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
    targets = benchmark_agrep_cli_end_to_end
);
criterion_main!(benches);
