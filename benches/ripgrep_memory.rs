//! Peak memory of byteseek and ripgrep on the `ripgrep_comparison` scenarios and, when their
//! corpora are available, the `ripgrep_benchsuite` ones.
//!
//! This is separate from the timed benches because it counts every allocation. Libraries are
//! measured by peak heap from building the searcher to the end of one count, excluding the
//! searched text (`grep-searcher` always allocates its line buffers up front). Command-line
//! tools are measured by peak RSS; the empty-file row shows the baseline process cost. On the
//! benchsuite corpora, they are also measured run the script's way: printing numbered lines,
//! on one thread for a single file and on default threads for the Linux tree.

use std::alloc::System;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use peakmem_alloc::{INSTRUMENTED_SYSTEM, PeakMemAlloc, PeakMemAllocTrait};
use wait4::Wait4;

#[path = "support/benchsuite.rs"]
mod benchsuite;
#[path = "support/cli.rs"]
mod cli;
#[path = "support/ripgrep.rs"]
mod comparison;
#[path = "support/logs.rs"]
mod logs;
#[path = "support/scenarios.rs"]
mod scenarios;

use benchsuite::{Corpus, NO_MMAP, Tool, for_each_corpus, printed_line_count};
use cli::{byteseek_path, checked_output, succeeded, write_corpus};
use comparison::{build_matcher, build_searcher, count_matching_lines, oracle_count};
use logs::{
    CLI_LINE_COUNT, IN_MEMORY_LINE_COUNTS, REAL_LOG_SCENARIOS, SYNTHETIC_SCENARIOS,
    build_log_corpus, real_log,
};
use scenarios::{
    Output, Scenario, build_rust_searcher, cli_command, printed_lines, ripgrep_cli_command,
    use_single_thread,
};

/// Counts heap bytes for every allocation in the process.
#[global_allocator]
static HEAP: &PeakMemAlloc<System> = &INSTRUMENTED_SYSTEM;

/// Runs per command-line tool; the largest peak is reported.
const CLI_RUNS: usize = 5;

/// Runs `search` and returns its count and the peak heap it added.
fn peak_heap(search: impl FnOnce() -> usize) -> (usize, usize) {
    HEAP.reset_peak_memory();
    let count = search();
    (count, HEAP.get_peak_memory())
}

/// Reads a child's pipe to the end. `run` names the run in panic messages.
fn read_pipe(pipe: Option<impl Read>, run: &str) -> Vec<u8> {
    let mut written = Vec::new();
    pipe.expect("the process's output is piped")
        .read_to_end(&mut written)
        .unwrap_or_else(|error| panic!("failed to read from {run}: {error}"));
    written
}

/// Runs a command [`CLI_RUNS`] times and returns the largest peak RSS in bytes. `run` names it
/// in panic messages.
///
/// Panics if any run fails or `counted` from its output isn't `expected`.
fn peak_resident_set(
    run: &str,
    command: impl Fn() -> Command,
    (counted, expected): (impl Fn(&[u8]) -> usize, usize),
) -> u64 {
    (0..CLI_RUNS)
        .map(|_| {
            let mut child = command()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap_or_else(|error| panic!("failed to execute {run}: {error}"));
            let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
            // Drain both pipes concurrently so the child can't block on a full one.
            let (printed, complaint) = thread::scope(|scope| {
                let complaint = scope.spawn(|| read_pipe(stderr, run));
                let printed = read_pipe(stdout, run);
                let complaint = complaint
                    .join()
                    .unwrap_or_else(|_| panic!("failed to read the errors of {run}"));
                (printed, complaint)
            });
            // `wait4` also returns the child's resource usage.
            let used = child
                .wait4()
                .unwrap_or_else(|error| panic!("failed to wait for {run}: {error}"));
            assert!(
                succeeded(used.status, &printed),
                "{run} failed with status {}: {}",
                used.status,
                String::from_utf8_lossy(&complaint).trim()
            );
            assert_eq!(counted(&printed), expected, "{run}");
            used.rusage.maxrss
        })
        .max()
        .expect("CLI_RUNS is positive")
}

const IN_MEMORY_TITLE: &str =
    "Peak heap in memory, searcher included, searched text excluded (KiB)";
const CLI_TITLE: &str =
    "Peak resident set size of the command-line tools, rg reading without a memory map (KiB)";
const LINES_TITLE: &str = "Peak resident set size printing numbered lines on equal cores, \
     rg reading without a memory map (KiB)";

/// Prints a Markdown table comparing two tools.
fn print_table(title: &str, tools: [&str; 2], rows: &[String]) {
    println!();
    println!("{title}");
    println!("| Scenario | {} | {} |", tools[0], tools[1]);
    println!("|---|---:|---:|");
    for row in rows {
        println!("{row}");
    }
}

/// Formats a table row, rounding each tool's bytes up to KiB.
fn row(scenario_name: &str, input_name: &str, bytes: [u64; 2]) -> String {
    let [rust, ripgrep] = bytes.map(|bytes| bytes.div_ceil(1024));
    format!("| {scenario_name}/{input_name} | {rust} | {ripgrep} |")
}

/// Measures both libraries counting matching lines of `corpus` in memory.
fn in_memory_row(
    scenario: &Scenario,
    (corpus, expected): (&str, usize),
    input_name: &str,
) -> String {
    let (rust_count, rust_heap) =
        peak_heap(|| build_rust_searcher(scenario).count_matching_lines(corpus));
    let (ripgrep_count, ripgrep_heap) = peak_heap(|| {
        let matcher = build_matcher(scenario.patterns, scenario.config());
        count_matching_lines(&mut build_searcher(), &matcher, corpus)
    });
    assert_eq!(rust_count, expected, "byteseek: {}", scenario.name);
    assert_eq!(ripgrep_count, expected, "ripgrep: {}", scenario.name);
    row(
        scenario.name,
        input_name,
        [rust_heap as u64, ripgrep_heap as u64],
    )
}

/// Measures both command-line tools counting matching lines of the file at `path`.
fn cli_row(scenario: &Scenario, (path, expected): (&Path, usize), input_name: &str) -> String {
    let rust_command = || cli_command(byteseek_path(), scenario, path, Output::Count);
    // Mapped pages belong to the page cache but would count toward rg's RSS.
    let ripgrep_command = || {
        let mut command = ripgrep_cli_command(scenario, path, Output::Count);
        command.arg("--no-mmap");
        command
    };
    let run = |tool: &str| format!("{tool}: {} on {input_name}", scenario.name);
    let counted = |tool| move |printed: &[u8]| Output::Count.matching_lines(tool, printed);
    let rust = peak_resident_set(
        &run("byteseek"),
        rust_command,
        (counted("byteseek"), expected),
    );
    let ripgrep = peak_resident_set(&run("rg"), ripgrep_command, (counted("rg"), expected));
    row(scenario.name, input_name, [rust, ripgrep])
}

/// Measures both command-line tools printing `scenario`'s numbered lines of `corpus` the
/// benchsuite script's way, checking they print as many lines.
fn lines_row(corpus: &Corpus, scenario: &Scenario) -> String {
    let ripgrep = Tool::Ripgrep(Some(NO_MMAP));
    let expected = printed_line_count(ripgrep.lines_command(corpus, scenario), "rg");
    let [rust, ripgrep] = [Tool::Byteseek, ripgrep].map(|tool| {
        peak_resident_set(
            &format!("{}: {} on {}", tool.name(), scenario.name, corpus.name),
            || tool.lines_command(corpus, scenario),
            (printed_lines, expected),
        )
    });
    row(scenario.name, corpus.name, [rust, ripgrep])
}

/// Adds a row per scenario to both tables, checking counts against the oracle. A `None`
/// path skips the command-line tools.
fn measure(
    scenarios: &[Scenario],
    (corpus, path): (&str, Option<&Path>),
    input_name: &str,
    (in_memory_rows, cli_rows): (&mut Vec<String>, &mut Vec<String>),
) {
    for scenario in scenarios {
        let expected = oracle_count(scenario.patterns, scenario.config(), corpus);
        in_memory_rows.push(in_memory_row(scenario, (corpus, expected), input_name));
        if let Some(path) = path {
            cli_rows.push(cli_row(scenario, (path, expected), input_name));
        }
    }
}

fn main() {
    // Match the timed comparison, and do it before anything is measured.
    use_single_thread();
    let mut version_command = Command::new("rg");
    version_command.arg("--version");
    let version = String::from_utf8_lossy(&checked_output(version_command, "rg")).into_owned();
    println!("rg is {}", version.lines().next().unwrap_or_default());
    let libraries = ["byteseek", "ripgrep (grep-searcher)"];
    let tools = ["byteseek", "rg"];

    let (mut in_memory_rows, mut cli_rows) = (Vec::new(), Vec::new());
    let empty = write_corpus("ripgrep_memory_empty", "");
    cli_rows.push(cli_row(
        &SYNTHETIC_SCENARIOS[0],
        (empty.path(), 0),
        "empty file",
    ));
    for line_count in IN_MEMORY_LINE_COUNTS {
        let corpus = build_log_corpus(line_count);
        let file =
            (line_count == CLI_LINE_COUNT).then(|| write_corpus("ripgrep_memory_logs", &corpus));
        // Borrow rather than move so the temporary file outlives the measurements.
        let path = file.as_ref().map(|file| file.path());
        measure(
            &SYNTHETIC_SCENARIOS,
            (&corpus, path),
            &line_count.to_string(),
            (&mut in_memory_rows, &mut cli_rows),
        );
    }
    if let Some(log) = real_log() {
        measure(
            &REAL_LOG_SCENARIOS,
            (&log.contents, Some(&log.path)),
            &log.name,
            (&mut in_memory_rows, &mut cli_rows),
        );
    }
    print_table(IN_MEMORY_TITLE, libraries, &in_memory_rows);
    print_table(CLI_TITLE, tools, &cli_rows);

    let (mut in_memory_rows, mut cli_rows, mut lines_rows) = (Vec::new(), Vec::new(), Vec::new());
    for_each_corpus(|corpus| {
        measure(
            corpus.scenarios,
            (&corpus.contents, Some(&corpus.path)),
            corpus.name,
            (&mut in_memory_rows, &mut cli_rows),
        );
        lines_rows.extend(
            corpus
                .scenarios
                .iter()
                .map(|scenario| lines_row(corpus, scenario)),
        );
    });
    if !in_memory_rows.is_empty() {
        print_table(IN_MEMORY_TITLE, libraries, &in_memory_rows);
        print_table(CLI_TITLE, tools, &cli_rows);
        print_table(LINES_TITLE, tools, &lines_rows);
    }
}
