//! Scenarios shared by the ripgrep comparisons, and the commands each tool runs them with.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use byteseek::{CaseMode, SearchConfig, Searcher};

/// One row of a comparison: a set of literal patterns and how they match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scenario {
    /// Name used in benchmark IDs and tables.
    pub name: &'static str,
    /// A line matches if it contains any of these.
    pub patterns: &'static [&'static str],
    /// Whether letters match in either case.
    pub case_mode: CaseMode,
    /// Require matches to be bounded by non-word ASCII bytes.
    pub whole_word: bool,
}

impl Scenario {
    /// Creates a scenario without whole-word matching.
    pub const fn new(
        name: &'static str,
        patterns: &'static [&'static str],
        case_mode: CaseMode,
    ) -> Self {
        Self {
            name,
            patterns,
            case_mode,
            whole_word: false,
        }
    }

    /// The byteseek configuration; ripgrep's matcher is derived from it.
    pub fn config(&self) -> SearchConfig {
        SearchConfig::builder()
            .case_mode(self.case_mode)
            .whole_word(self.whole_word)
            .build()
    }
}

/// Limits the global rayon pool to one thread. Panics if the pool was already built.
pub fn use_single_thread() {
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global()
        .expect("single-thread comparison pool should initialize");
}

/// Compiles the byteseek searcher for `scenario`.
pub fn build_rust_searcher(scenario: &Scenario) -> Searcher {
    Searcher::new(scenario.patterns.iter().copied(), scenario.config())
        .expect("byteseek searcher should compile")
}

/// Builds a command for `program`, which takes ripgrep's flags, that searches for `scenario`'s
/// literal patterns with its case and word settings and prints what the `output` flags select.
/// The caller adds what to search.
pub fn search_command(program: impl AsRef<OsStr>, scenario: &Scenario, output: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.args(output);
    match scenario.case_mode {
        CaseMode::Sensitive => {}
        CaseMode::AsciiInsensitive => {
            command.arg("--ignore-case");
        }
    }
    if scenario.whole_word {
        command.arg("--word-regexp");
    }
    for pattern in scenario.patterns {
        command.arg("--regexp").arg(pattern);
    }
    command
}

/// What a command-line run prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The count of matching lines.
    Count,
    /// Each matching line with its line number.
    NumberedLines,
}

impl Output {
    /// The flags that select this output.
    pub fn flags(self) -> &'static [&'static str] {
        match self {
            Self::Count => &["--count", "--no-filename"],
            Self::NumberedLines => &["--line-number"],
        }
    }

    /// The matching lines that `printed`, output of `tool`, reports, panicking if a count
    /// isn't one.
    pub fn matching_lines(self, tool: &str, printed: &[u8]) -> usize {
        match self {
            Self::Count => parsed_count(tool, &String::from_utf8_lossy(printed)),
            Self::NumberedLines => printed_lines(printed),
        }
    }
}

/// Builds the [`search_command`] that prints `output` for `path` on one thread.
pub fn cli_command(
    program: impl AsRef<OsStr>,
    scenario: &Scenario,
    path: &Path,
    output: Output,
) -> Command {
    let mut command = search_command(program, scenario, output.flags());
    command
        .args(["--fixed-strings", "--threads", "1"])
        .arg(path);
    command
}

/// Gives an `rg` command byteseek's ASCII-only case folding and word bytes, and no config file.
pub fn match_like_byteseek(command: &mut Command) -> &mut Command {
    command.args(["--no-config", "--no-unicode"])
}

/// Builds the [`cli_command`] of `rg`, matching like byteseek.
pub fn ripgrep_cli_command(scenario: &Scenario, path: &Path, output: Output) -> Command {
    let mut command = cli_command("rg", scenario, path, output);
    match_like_byteseek(&mut command);
    command
}

/// Parses a count printed with [`Output::Count`], where no output means zero, panicking with
/// `tool` in the message if it isn't one.
pub fn parsed_count(tool: &str, printed: &str) -> usize {
    let printed = printed.trim();
    if printed.is_empty() {
        return 0;
    }
    printed
        .parse()
        .unwrap_or_else(|error| panic!("{tool} printed {printed:?}, not a count: {error}"))
}

/// Counts the lines `printed`, by their line feeds.
pub fn printed_lines(printed: &[u8]) -> usize {
    memchr::memchr_iter(b'\n', printed).count()
}
