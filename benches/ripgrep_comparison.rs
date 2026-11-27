use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main};

#[path = "support/cli.rs"]
mod cli;
#[path = "support/ripgrep.rs"]
mod comparison;
#[path = "support/logs.rs"]
mod logs;
#[path = "support/scenarios.rs"]
mod scenarios;
#[path = "support/text.rs"]
mod text;
#[path = "support/timed.rs"]
mod timed;

use cli::write_corpus;
use logs::{
    CLI_LINE_COUNT, IN_MEMORY_LINE_COUNTS, REAL_LOG_SCENARIOS, SYNTHETIC_SCENARIOS,
    build_log_corpus, real_log,
};
use scenarios::{Output, Scenario, use_single_thread};
use text::{UTF8_LINE_COUNT, UTF8_SCENARIOS, build_utf8_corpus};
use timed::{bench_cli, bench_in_memory, run_order, verified_line_count};

fn benchmark_ripgrep_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("ripgrep_comparison_single_thread_in_memory");
    let order = run_order();

    for line_count in IN_MEMORY_LINE_COUNTS {
        let corpus = build_log_corpus(line_count);
        let input_name = line_count.to_string();
        group.throughput(Throughput::Bytes(corpus.len() as u64));
        for scenario in &SYNTHETIC_SCENARIOS {
            bench_in_memory(&mut group, scenario, &corpus, &input_name, order);
        }
    }
    if let Some(log) = real_log() {
        group.throughput(Throughput::Bytes(log.contents.len() as u64));
        for scenario in &REAL_LOG_SCENARIOS {
            bench_in_memory(&mut group, scenario, &log.contents, &log.name, order);
        }
    }

    group.finish();
}

/// Times both command-line tools printing `output` for `scenarios` on `corpus`, written to a
/// temporary file.
fn bench_cli_corpus(
    group: &mut BenchmarkGroup<'_, WallTime>,
    (corpus, input_name): (&str, &str),
    scenarios: &[Scenario],
    output: Output,
) {
    let order = run_order();
    let file = write_corpus("ripgrep_cli_corpus", corpus);
    group.throughput(Throughput::Bytes(corpus.len() as u64));
    for scenario in scenarios {
        let expected = verified_line_count(scenario, corpus, input_name);
        let input = (file.path(), expected, output);
        bench_cli(group, scenario, input, input_name, order);
    }
}

/// Times both command-line tools on the synthetic log, the multilingual text and, if given,
/// the real log: counting matching lines, then printing them with their numbers.
fn benchmark_ripgrep_cli(c: &mut Criterion) {
    let logs = build_log_corpus(CLI_LINE_COUNT);
    let text = build_utf8_corpus(UTF8_LINE_COUNT);
    let log = real_log();
    for (group_name, output) in [
        (
            "ripgrep_cli_end_to_end_single_thread_warm_cache",
            Output::Count,
        ),
        (
            "ripgrep_cli_numbered_lines_single_thread_warm_cache",
            Output::NumberedLines,
        ),
    ] {
        let mut group = c.benchmark_group(group_name);
        let logs_name = CLI_LINE_COUNT.to_string();
        bench_cli_corpus(
            &mut group,
            (&logs, &logs_name),
            &SYNTHETIC_SCENARIOS,
            output,
        );
        bench_cli_corpus(&mut group, (&text, "utf8"), &UTF8_SCENARIOS, output);
        if let Some(log) = &log {
            bench_cli_corpus(
                &mut group,
                (&log.contents, &log.name),
                &REAL_LOG_SCENARIOS,
                output,
            );
        }
        group.finish();
    }
}

fn criterion_config() -> Criterion {
    use_single_thread();
    Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2))
        .sample_size(20)
}

criterion_group!(
    name = benches;
    config = criterion_config();
    targets = benchmark_ripgrep_comparison, benchmark_ripgrep_cli
);
criterion_main!(benches);
