//! Synthetic and real log corpora, and their scenarios, for the ripgrep comparisons.

use std::env;
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

use byteseek::CaseMode;

use crate::scenarios::Scenario;

/// Synthetic log sizes, in lines, searched in memory.
pub const IN_MEMORY_LINE_COUNTS: [usize; 2] = [20_000, 120_000];
/// Synthetic log size, in lines, searched by the command-line tools.
pub const CLI_LINE_COUNT: usize = 120_000;
/// Optional real log file to also compare on, e.g. `/var/log/install.log`.
const REAL_LOG_ENV: &str = "BYTESEEK_REAL_LOG";

const SENSITIVE_SINGLE: [&str; 1] = ["ERROR"];
const INSENSITIVE_SINGLE: [&str; 1] = ["warn"];
const SENSITIVE_MULTI: [&str; 4] = ["ERROR", "WARN", "status=500", "request_id=1024"];
const INSENSITIVE_MULTI: [&str; 4] = ["error", "warn", "trace", "status=500"];
// Sparse patterns match under 0.1% of lines, typical of real grep searches.
const SPARSE_SENSITIVE_SINGLE: [&str; 1] = ["request_id=1999"];
const SPARSE_INSENSITIVE_MULTI: [&str; 3] = ["REQUEST_ID=1999", "Request_Id=7777", "STATUS=404"];
const SENSITIVE_SIX: [&str; 6] = [
    "ERROR",
    "WARN",
    "TRACE",
    "status=500",
    "request_id=1024",
    "request_id=77",
];
const SPARSE_INSENSITIVE_SIX: [&str; 6] = [
    "REQUEST_ID=1999",
    "Request_Id=7777",
    "STATUS=404",
    "request_id=4242",
    "Request_ID=31337",
    "status=503",
];
// Patterns sharing prefixes, as fields of one log format do.
const SHARED_PREFIX_SIX: [&str; 6] = [
    "ERROR",
    "status=204",
    "status=207",
    "request_id=1999",
    "request_id=77",
    "id=59",
];
const SENSITIVE_EIGHT: [&str; 8] = [
    "ERROR",
    "WARN",
    "TRACE",
    "status=500",
    "request_id=1024",
    "request_id=77",
    "INFO",
    "debug",
];

/// Scenarios run on the log from [`build_log_corpus`].
pub const SYNTHETIC_SCENARIOS: [Scenario; 10] = [
    Scenario::new("single_sensitive", &SENSITIVE_SINGLE, CaseMode::Sensitive),
    Scenario::new(
        "single_ascii_insensitive",
        &INSENSITIVE_SINGLE,
        CaseMode::AsciiInsensitive,
    ),
    Scenario::new("multi_sensitive", &SENSITIVE_MULTI, CaseMode::Sensitive),
    Scenario::new(
        "multi_ascii_insensitive",
        &INSENSITIVE_MULTI,
        CaseMode::AsciiInsensitive,
    ),
    Scenario::new(
        "sparse_single_sensitive",
        &SPARSE_SENSITIVE_SINGLE,
        CaseMode::Sensitive,
    ),
    Scenario::new(
        "sparse_multi_ascii_insensitive",
        &SPARSE_INSENSITIVE_MULTI,
        CaseMode::AsciiInsensitive,
    ),
    Scenario::new("six_sensitive", &SENSITIVE_SIX, CaseMode::Sensitive),
    Scenario::new(
        "sparse_six_ascii_insensitive",
        &SPARSE_INSENSITIVE_SIX,
        CaseMode::AsciiInsensitive,
    ),
    Scenario::new(
        "six_shared_prefix_sensitive",
        &SHARED_PREFIX_SIX,
        CaseMode::Sensitive,
    ),
    Scenario::new("eight_sensitive", &SENSITIVE_EIGHT, CaseMode::Sensitive),
];

// Chosen for a macOS installer log; other files give other match densities.
const REAL_DENSE_SINGLE: [&str; 1] = ["softwareupdated"];
const REAL_SPARSE_SINGLE: [&str; 1] = ["receipt"];
const REAL_INSENSITIVE_SINGLE: [&str; 1] = ["failed"];
const REAL_SENSITIVE_MULTI: [&str; 3] = ["installd", "PackageKit", "Installer"];
const REAL_INSENSITIVE_MULTI: [&str; 3] = ["error", "failed", "warning"];
const REAL_INSENSITIVE_SIX: [&str; 6] = [
    "error",
    "failed",
    "warning",
    "denied",
    "timed out",
    "unable to",
];

/// Scenarios run on the real log file, if one is given.
pub const REAL_LOG_SCENARIOS: [Scenario; 6] = [
    Scenario::new(
        "dense_single_sensitive",
        &REAL_DENSE_SINGLE,
        CaseMode::Sensitive,
    ),
    Scenario::new(
        "sparse_single_sensitive",
        &REAL_SPARSE_SINGLE,
        CaseMode::Sensitive,
    ),
    Scenario::new(
        "single_ascii_insensitive",
        &REAL_INSENSITIVE_SINGLE,
        CaseMode::AsciiInsensitive,
    ),
    Scenario::new(
        "multi_sensitive",
        &REAL_SENSITIVE_MULTI,
        CaseMode::Sensitive,
    ),
    Scenario::new(
        "multi_ascii_insensitive",
        &REAL_INSENSITIVE_MULTI,
        CaseMode::AsciiInsensitive,
    ),
    Scenario::new(
        "six_ascii_insensitive",
        &REAL_INSENSITIVE_SIX,
        CaseMode::AsciiInsensitive,
    ),
];

/// A real log file named by `BYTESEEK_REAL_LOG`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealLog {
    /// File name, used in benchmark IDs and table rows.
    pub name: String,
    pub path: PathBuf,
    pub contents: String,
}

/// Reads the log named by `BYTESEEK_REAL_LOG`, or returns `None` if it is unset.
pub fn real_log() -> Option<RealLog> {
    let path = PathBuf::from(env::var_os(REAL_LOG_ENV)?);
    let contents = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{REAL_LOG_ENV}={}: {error}", path.display()));
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    Some(RealLog {
        name,
        path,
        contents,
    })
}

/// Builds a deterministic log of `lines` lines with mixed-case levels, request IDs and status
/// codes.
pub fn build_log_corpus(lines: usize) -> String {
    let mut corpus = String::with_capacity(lines * 96);
    for index in 0..lines {
        let level = match index % 8 {
            0 => "INFO",
            1 => "debug",
            2 => "WARN",
            3 => "Error",
            4 => "TRACE",
            5 => "info",
            6 => "warn",
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
        // `write!` formats in place, avoiding a `to_string` allocation per line.
        write!(corpus, "{index}").expect("writing to a string cannot fail");
        corpus.push_str(" status=");
        corpus.push_str(if index % 17 == 0 { "500" } else { "200" });
        corpus.push_str(" message=handled user event\n");
    }
    corpus
}

/// Appends `value` (below 100) as two decimal digits.
fn push_two_digits(output: &mut String, value: u8) {
    output.push(char::from(b'0' + (value / 10)));
    output.push(char::from(b'0' + (value % 10)));
}
