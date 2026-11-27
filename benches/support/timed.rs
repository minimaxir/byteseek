//! Timed ripgrep comparisons: both tools count matching lines in memory and as command-line
//! tools, or print them as command-line tools, after their counts are checked against an
//! independent oracle.

use std::env;
use std::hint::black_box;
use std::path::Path;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, BenchmarkId};

use crate::cli::{byteseek_path, checked_output};
use crate::comparison::{build_matcher, build_searcher, count_matching_lines, oracle_count};
use crate::scenarios::{Output, Scenario, build_rust_searcher, cli_command, ripgrep_cli_command};

/// For each of a row's two runs, whether it is byteseek's. Set `BYTESEEK_BENCH_REVERSE` to
/// run ripgrep first and check that run order doesn't skew results.
pub fn run_order() -> [bool; 2] {
    if env::var_os("BYTESEEK_BENCH_REVERSE").is_some() {
        [false, true]
    } else {
        [true, false]
    }
}

/// Asserts both libraries agree with the oracle on `corpus` and returns the count.
pub fn verified_line_count(scenario: &Scenario, corpus: &str, input_name: &str) -> usize {
    let rust_searcher = build_rust_searcher(scenario);
    let matcher = build_matcher(scenario.patterns, scenario.config());
    let expected = oracle_count(scenario.patterns, scenario.config(), corpus);

    let rust_count = rust_searcher.count_matching_lines(corpus);
    let ripgrep_count = count_matching_lines(&mut build_searcher(), &matcher, corpus);

    let context = format!("{} on {input_name}", scenario.name);
    assert_eq!(
        rust_count, expected,
        "byteseek differs from the oracle: {context}"
    );
    assert_eq!(
        ripgrep_count, expected,
        "ripgrep differs from the oracle: {context}"
    );
    expected
}

/// Times both libraries counting matching lines of `corpus` in memory. Returns the verified
/// count.
pub fn bench_in_memory(
    group: &mut BenchmarkGroup<'_, WallTime>,
    scenario: &Scenario,
    corpus: &str,
    input_name: &str,
    order: [bool; 2],
) -> usize {
    let expected = verified_line_count(scenario, corpus, input_name);
    let rust_searcher = build_rust_searcher(scenario);
    let matcher = build_matcher(scenario.patterns, scenario.config());

    for rust_first in order {
        if rust_first {
            group.bench_function(
                BenchmarkId::new(format!("byteseek/{}", scenario.name), input_name),
                |bench| {
                    bench.iter(|| {
                        black_box(black_box(&rust_searcher).count_matching_lines(black_box(corpus)))
                    });
                },
            );
        } else {
            group.bench_function(
                BenchmarkId::new(format!("ripgrep/{}", scenario.name), input_name),
                |bench| {
                    let mut ripgrep_searcher = build_searcher();
                    bench.iter(|| {
                        black_box(count_matching_lines(
                            black_box(&mut ripgrep_searcher),
                            black_box(&matcher),
                            black_box(corpus),
                        ))
                    });
                },
            );
        }
    }
    expected
}

/// Prints `output` for the file at `path` with `byteseek` and returns the matching lines.
fn run_rust_cli(scenario: &Scenario, path: &Path, output: Output) -> usize {
    let command = cli_command(byteseek_path(), scenario, path, output);
    output.matching_lines("byteseek", &checked_output(command, "byteseek"))
}

/// Runs a command-line tool on a scenario and file, returning the matching lines it printed.
type RunCli = fn(&Scenario, &Path, Output) -> usize;

/// Prints `output` for the file at `path` with `rg` and returns the matching lines.
fn run_ripgrep_cli(scenario: &Scenario, path: &Path, output: Output) -> usize {
    let tool = "rg (is ripgrep installed?)";
    let command = ripgrep_cli_command(scenario, path, output);
    output.matching_lines(tool, &checked_output(command, tool))
}

/// Times both command-line tools printing `output` for the file at `path`, after checking
/// each reports `expected` matching lines.
pub fn bench_cli(
    group: &mut BenchmarkGroup<'_, WallTime>,
    scenario: &Scenario,
    (path, expected, output): (&Path, usize, Output),
    input_name: &str,
    order: [bool; 2],
) {
    let context = format!("{} on {input_name}", scenario.name);
    assert_eq!(
        run_rust_cli(scenario, path, output),
        expected,
        "byteseek CLI: {context}"
    );
    assert_eq!(
        run_ripgrep_cli(scenario, path, output),
        expected,
        "rg CLI: {context}"
    );

    for rust_first in order {
        let (tool, run): (&str, RunCli) = if rust_first {
            ("byteseek", run_rust_cli)
        } else {
            ("ripgrep", run_ripgrep_cli)
        };
        group.bench_function(
            BenchmarkId::new(format!("{tool}/{}", scenario.name), input_name),
            |bench| bench.iter(|| black_box(run(black_box(scenario), black_box(path), output))),
        );
    }
}
