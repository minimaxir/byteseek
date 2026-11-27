//! Corpora and literal benchmarks from ripgrep's `benchsuite` script.
//!
//! Set `BYTESEEK_BENCHSUITE_DIR` to the directory filled by
//! `benchsuite --dir <directory> --download all` (the Linux tree need not be built). When it is
//! unset, nothing is searched.
//!
//! Regex benchmarks, plus the Russian case-insensitive and whole-word ones (Unicode rules in
//! ripgrep, ASCII in byteseek), are omitted, and `rg` gets `--no-unicode` to match ASCII rules
//! like byteseek. Alternates are passed as one `--regexp` each rather than joined with `|`.
//!
//! The library and count benchmarks differ from the script: both tools count lines on one
//! thread instead of printing them, and the Linux tree's text files are joined into one text.
//! [`Tool::lines_command`] runs the script's own method instead: each tool prints matching
//! lines with their numbers into a pipe, the Linux tree is searched as the working directory
//! with default ignore rules, and the output's lines are counted. Both tools get the same cores
//! to use: one for a single file, since `rg` searches a file on one thread whatever its thread
//! count, and their default threads for the tree, whose files both search in parallel.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use byteseek::CaseMode;
use tempfile::NamedTempFile;

use crate::cli::{byteseek_path, checked_output, write_corpus};
use crate::scenarios::{Output, Scenario, match_like_byteseek, search_command};

/// Names the directory the `benchsuite` script downloaded into.
const BENCHSUITE_ENV: &str = "BYTESEEK_BENCHSUITE_DIR";

const ENGLISH_LITERAL: [&str; 1] = ["Sherlock Holmes"];
const ENGLISH_ALTERNATES: [&str; 5] = [
    "Sherlock Holmes",
    "John Watson",
    "Irene Adler",
    "Inspector Lestrade",
    "Professor Moriarty",
];
// The English names as spelled in the Russian subtitles.
const RUSSIAN_LITERAL: [&str; 1] = ["Шерлок Холмс"];
const RUSSIAN_ALTERNATES: [&str; 5] = [
    "Шерлок Холмс",
    "Джон Уотсон",
    "Ирен Адлер",
    "инспектор Лестрейд",
    "профессор Мориарти",
];
const LINUX_LITERAL: [&str; 1] = ["PM_RESUME"];
const LINUX_ALTERNATES: [&str; 4] = ["ERR_SYS", "PME_TURN_OFF", "LINK_REQ_RST", "CFG_BME_EVT"];

/// The script's five literal benchmarks for a corpus with ASCII patterns.
const fn ascii_scenarios(
    literal: &'static [&'static str],
    alternates: &'static [&'static str],
) -> [Scenario; 5] {
    [
        Scenario::new("literal", literal, CaseMode::Sensitive),
        Scenario::new("literal_casei", literal, CaseMode::AsciiInsensitive),
        Scenario {
            whole_word: true,
            ..Scenario::new("literal_word", literal, CaseMode::Sensitive)
        },
        Scenario::new("alternates", alternates, CaseMode::Sensitive),
        Scenario::new("alternates_casei", alternates, CaseMode::AsciiInsensitive),
    ]
}

const ENGLISH_SCENARIOS: [Scenario; 5] = ascii_scenarios(&ENGLISH_LITERAL, &ENGLISH_ALTERNATES);
const RUSSIAN_SCENARIOS: [Scenario; 2] = [
    Scenario::new("literal", &RUSSIAN_LITERAL, CaseMode::Sensitive),
    Scenario::new("alternates", &RUSSIAN_ALTERNATES, CaseMode::Sensitive),
];
const LINUX_SCENARIOS: [Scenario; 5] = ascii_scenarios(&LINUX_LITERAL, &LINUX_ALTERNATES);

/// A labeled `rg` row the script adds to some benchmarks, reading with another I/O strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoVariant {
    /// Tool name in benchmark IDs.
    pub name: &'static str,
    /// The `rg` flag that selects the strategy.
    pub flag: &'static str,
}

/// The script's `rg (no mmap)` row.
pub const NO_MMAP: IoVariant = IoVariant {
    name: "ripgrep_no_mmap",
    flag: "--no-mmap",
};

/// A loaded corpus and the benchmarks run on it.
#[derive(Debug)]
pub struct Corpus {
    /// Name used in benchmark IDs and tables.
    pub name: &'static str,
    pub scenarios: &'static [Scenario],
    pub contents: String,
    /// File the command-line tools search when counting.
    pub path: PathBuf,
    /// Directory tree the script searches as its working directory, if the corpus is one.
    tree: Option<PathBuf>,
    /// Keeps a joined tree's temporary file alive until the corpus is dropped.
    _joined: Option<NamedTempFile>,
}

impl Corpus {
    /// Loads a single-file corpus, panicking if it isn't readable UTF-8.
    fn of_file(name: &'static str, scenarios: &'static [Scenario], path: PathBuf) -> Self {
        let contents = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{BENCHSUITE_ENV}: {}: {error}", path.display()));
        Self {
            name,
            scenarios,
            contents,
            path,
            tree: None,
            _joined: None,
        }
    }

    /// Loads a directory tree as one text, written to a temporary file for counting.
    fn of_tree(name: &'static str, scenarios: &'static [Scenario], root: PathBuf) -> Self {
        let contents = joined_text_files(&root);
        let joined = write_corpus("ripgrep_benchsuite_joined", &contents);
        Self {
            name,
            scenarios,
            contents,
            path: joined.path().to_path_buf(),
            tree: Some(root),
            _joined: Some(joined),
        }
    }

    /// Builds the script's command for `program`, which takes ripgrep's flags: print each
    /// line matching `scenario` with its line number, searching the file on one thread or,
    /// for a tree, the working directory on the tool's default threads.
    fn lines_command(&self, program: impl AsRef<OsStr>, scenario: &Scenario) -> Command {
        let mut command = search_command(program, scenario, Output::NumberedLines.flags());
        match &self.tree {
            Some(root) => command.current_dir(root),
            None => command.args(["--threads", "1"]).arg(&self.path),
        };
        command
    }
}

/// A command-line tool run the script's way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Byteseek,
    /// `rg`, matching like byteseek, reading with its default I/O or a labeled variant's.
    Ripgrep(Option<IoVariant>),
}

impl Tool {
    /// Name in benchmark IDs and messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Byteseek => "byteseek",
            Self::Ripgrep(None) => "ripgrep",
            Self::Ripgrep(Some(variant)) => variant.name,
        }
    }

    /// Builds the [`Corpus`]'s command for this tool printing `scenario`'s numbered lines.
    pub fn lines_command(self, corpus: &Corpus, scenario: &Scenario) -> Command {
        match self {
            Self::Byteseek => corpus.lines_command(byteseek_path(), scenario),
            Self::Ripgrep(variant) => {
                let mut command = corpus.lines_command("rg", scenario);
                match_like_byteseek(&mut command).args(variant.map(|variant| variant.flag));
                command
            }
        }
    }
}

/// Runs `command` and returns how many lines it printed, as the script counts them,
/// panicking if it fails.
pub fn printed_line_count(command: Command, tool: &str) -> usize {
    Output::NumberedLines.matching_lines(tool, &checked_output(command, tool))
}

/// Concatenates the tree's text files in path order.
///
/// Text means UTF-8 without NUL bytes (`rg`'s binary heuristic). Skips `.git` and symlinks,
/// and terminates each file's last line so lines don't run across files.
fn joined_text_files(root: &Path) -> String {
    let unreadable = |path: &Path, error| -> ! { panic!("{}: {error}", path.display()) };
    let mut directories = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = directories.pop() {
        let entries =
            fs::read_dir(&directory).unwrap_or_else(|error| unreadable(&directory, error));
        for entry in entries {
            let entry = entry.unwrap_or_else(|error| unreadable(&directory, error));
            let path = entry.path();
            // Not followed through symlinks, so links are neither files nor directories.
            let file_type = entry
                .file_type()
                .unwrap_or_else(|error| unreadable(&path, error));
            if file_type.is_file() {
                files.push(path);
            } else if file_type.is_dir() && entry.file_name() != ".git" {
                directories.push(path);
            }
        }
    }
    files.sort_unstable();

    let mut joined = String::new();
    for path in files {
        let bytes = fs::read(&path).unwrap_or_else(|error| unreadable(&path, error));
        // Validates in place; no copy.
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        if text.contains('\0') {
            continue;
        }
        joined.push_str(&text);
        if !text.is_empty() && !text.ends_with('\n') {
            joined.push('\n');
        }
    }
    joined
}

/// Loads each corpus in turn and passes it to `visit`, so only one is in memory at a time.
/// Does nothing when `BYTESEEK_BENCHSUITE_DIR` is unset.
pub fn for_each_corpus(mut visit: impl FnMut(&Corpus)) {
    let Some(directory) = env::var_os(BENCHSUITE_ENV).map(PathBuf::from) else {
        eprintln!("{BENCHSUITE_ENV} is not set: no benchsuite corpus is searched");
        return;
    };
    let subtitles = directory.join("subtitles");
    visit(&Corpus::of_file(
        "subtitles_en",
        &ENGLISH_SCENARIOS,
        subtitles.join("en.sample.txt"),
    ));
    visit(&Corpus::of_file(
        "subtitles_ru",
        &RUSSIAN_SCENARIOS,
        subtitles.join("ru.txt"),
    ));
    visit(&Corpus::of_tree(
        "linux",
        &LINUX_SCENARIOS,
        directory.join("linux"),
    ));
}
