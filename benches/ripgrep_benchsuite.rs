//! Times byteseek and ripgrep on the corpora of ripgrep's `benchsuite` script (English and
//! Russian subtitles, Linux source). Requires `BYTESEEK_BENCHSUITE_DIR`; see
//! `support/benchsuite.rs` for which benchmarks run and how they differ.
//!
//! Three groups run: the libraries counting lines in memory on one thread, the command-line
//! tools counting lines on one thread, and the command-line tools run the script's way, printing
//! numbered lines on the same cores.

use std::hint::black_box;
use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{
    BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};

#[path = "support/benchsuite.rs"]
mod benchsuite;
#[path = "support/cli.rs"]
mod cli;
#[path = "support/ripgrep.rs"]
mod comparison;
#[path = "support/scenarios.rs"]
mod scenarios;
#[path = "support/timed.rs"]
mod timed;

use benchsuite::{Corpus, IoVariant, NO_MMAP, Tool, for_each_corpus, printed_line_count};
use scenarios::{Output, Scenario, use_single_thread};
use timed::{bench_cli, bench_in_memory, run_order};

/// The script's `rg (mmap)` row.
const MMAP: IoVariant = IoVariant {
    name: "ripgrep_mmap",
    flag: "--mmap",
};

/// The labeled `rg` row the script adds to `scenario` on `corpus`, if any.
fn io_variant(corpus: &Corpus, scenario: &Scenario) -> Option<IoVariant> {
    match (corpus.name, scenario.name) {
        ("linux", "literal" | "literal_casei") => Some(MMAP),
        ("subtitles_en" | "subtitles_ru", "literal") => Some(NO_MMAP),
        _ => None,
    }
}

/// Times the command-line tools printing `scenario`'s numbered lines of `corpus` the script's
/// way, then `rg`'s labeled I/O row if the script has one, after checking that each prints as
/// many lines as `rg`.
fn bench_lines(
    group: &mut BenchmarkGroup<'_, WallTime>,
    corpus: &Corpus,
    scenario: &Scenario,
    byteseek_first: bool,
) {
    let ripgrep = Tool::Ripgrep(None);
    let mut tools = vec![Tool::Byteseek, ripgrep];
    if !byteseek_first {
        tools.reverse();
    }
    tools.extend(io_variant(corpus, scenario).map(|variant| Tool::Ripgrep(Some(variant))));
    let expected = printed_line_count(ripgrep.lines_command(corpus, scenario), "rg");
    for tool in tools.iter().filter(|&&tool| tool != ripgrep) {
        assert_eq!(
            printed_line_count(tool.lines_command(corpus, scenario), tool.name()),
            expected,
            "{} printed a different number of lines than rg: {} on {}",
            tool.name(),
            scenario.name,
            corpus.name
        );
    }
    for tool in tools {
        group.bench_function(
            BenchmarkId::new(format!("{}/{}", tool.name(), scenario.name), corpus.name),
            |bench| {
                bench.iter(|| {
                    black_box(printed_line_count(
                        tool.lines_command(corpus, scenario),
                        tool.name(),
                    ))
                });
            },
        );
    }
}

fn benchmark_benchsuite(c: &mut Criterion) {
    let order = run_order();
    for_each_corpus(|corpus| {
        let throughput = Throughput::Bytes(corpus.contents.len() as u64);

        let mut group = c.benchmark_group("ripgrep_benchsuite_single_thread_in_memory");
        group.throughput(throughput.clone());
        // Reuse the oracle counts for the CLI group; computing them takes seconds.
        let counts: Vec<usize> = corpus
            .scenarios
            .iter()
            .map(|scenario| {
                bench_in_memory(&mut group, scenario, &corpus.contents, corpus.name, order)
            })
            .collect();
        group.finish();

        let mut group =
            c.benchmark_group("ripgrep_benchsuite_cli_end_to_end_single_thread_warm_cache");
        group.throughput(throughput.clone());
        for (scenario, expected) in corpus.scenarios.iter().zip(counts) {
            let input = (corpus.path.as_path(), expected, Output::Count);
            bench_cli(&mut group, scenario, input, corpus.name, order);
        }
        group.finish();

        let mut group =
            c.benchmark_group("ripgrep_benchsuite_cli_ripgrep_method_equal_cores_warm_cache");
        group.throughput(throughput);
        for scenario in corpus.scenarios {
            bench_lines(&mut group, corpus, scenario, order[0]);
        }
        group.finish();
    });
}

/// Each search takes up to seconds, so use criterion's minimum sample size.
fn criterion_config() -> Criterion {
    use_single_thread();
    Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(10)
}

criterion_group!(
    name = benches;
    config = criterion_config();
    targets = benchmark_benchsuite
);
criterion_main!(benches);
