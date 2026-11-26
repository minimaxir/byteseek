//! Command-line flags, named and defaulted as ripgrep names and defaults them.

use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::FromStr;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{ArgAction, Parser, ValueEnum};

use crate::cli::style;
use crate::{CaseMode, StructuredMode, StructuredPredicateHandling};

/// Help colors in the convention of Cargo and other Rust tools.
const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD));

/// The ways to call `byteseek`, styled like the usage clap writes.
fn usage() -> String {
    let literal = HELP_STYLES.get_literal();
    let placeholder = HELP_STYLES.get_placeholder();
    [
        "byteseek [OPTIONS] PATTERN [PATH...]",
        "byteseek [OPTIONS] -e PATTERN... [PATH...]",
        "byteseek [OPTIONS] -f PATTERNFILE... [PATH...]",
    ]
    .map(|line| {
        line.split(' ')
            .map(|word| {
                let style = if word == "byteseek" || word.starts_with('-') {
                    literal
                } else {
                    placeholder
                };
                format!("{style}{word}{style:#}")
            })
            .collect::<Vec<_>>()
            .join(" ")
    })
    .join("\n       ")
}

/// Commands and what they do, shown after `--help`.
const EXAMPLES: [(&str, &str); 6] = [
    ("byteseek ERROR", "Search the current directory"),
    (
        "byteseek -p -e ERROR -e timeout logs",
        "Lay out two patterns for reading",
    ),
    ("byteseek -k 1 recieve src", "Also match within one typo"),
    (
        "byteseek --query 'ERROR AND NOT retry' logs",
        "Select lines by boolean query",
    ),
    (
        "byteseek --parse json '$.latency_ms > 500' data",
        "Select JSON values by predicate",
    ),
    (
        "byteseek --report -e ERROR -e WARN logs",
        "Summarize matches in tables",
    ),
];

/// The examples section of `--help`, styled like the sections clap writes.
fn examples() -> String {
    let header = HELP_STYLES.get_header();
    let literal = HELP_STYLES.get_literal();
    let width = EXAMPLES
        .iter()
        .map(|(command, _)| command.len())
        .max()
        .unwrap_or(0);
    let rows: String = EXAMPLES
        .iter()
        .map(|(command, purpose)| format!("\n  {literal}{command:<width$}{literal:#}  {purpose}"))
        .collect();
    format!("{AFTER_HELP}\n\n{header}Examples:{header:#}{rows}")
}

/// Notes shown after the flags in both `-h` and `--help`.
const AFTER_HELP: &str = "Patterns are literal text. A regular expression is accepted only when \
                          it matches one fixed string, like foo\\.bar; pass -F to search for any \
                          other as written. Exit status is 0 if a line was selected, 1 if none \
                          was, and 2 on error.";

/// Recursively search the current directory for lines matching literal patterns, with
/// The output modes of which, as in ripgrep, the last given applies.
const SUMMARIES: [&str; 4] = [
    "count",
    "count_matches",
    "files_with_matches",
    "files_without_match",
];

/// ripgrep's flags and output.
#[derive(Debug, Parser)]
#[command(
    name = "byteseek",
    version,
    args_override_self = true,
    override_usage = usage(),
    styles = HELP_STYLES,
    after_help = AFTER_HELP,
    after_long_help = examples()
)]
pub struct Args {
    /// PATTERN unless -e or -f is given, then the files and directories to search. Without
    /// paths, reads stdin when it is piped and searches "./" otherwise; "-" reads stdin.
    #[arg(value_names = ["PATTERN", "PATH"], num_args = 0..)]
    positionals: Vec<OsString>,

    /// A pattern to search for; a line matches if any pattern does. Repeatable.
    #[arg(
        short = 'e',
        long = "regexp",
        value_name = "PATTERN",
        help_heading = "Input"
    )]
    patterns: Vec<String>,
    /// Read patterns from a file, one per line. Repeatable.
    #[arg(
        short = 'f',
        long = "file",
        value_name = "PATTERNFILE",
        help_heading = "Input"
    )]
    pattern_files: Vec<PathBuf>,

    /// Treat patterns as literal strings without parsing them as regular expressions.
    #[arg(short = 'F', long, help_heading = "Search")]
    fixed_strings: bool,
    /// Fold ASCII case when matching.
    #[arg(short = 'i', long, overrides_with_all = ["case_sensitive", "smart_case"],
        help_heading = "Search")]
    ignore_case: bool,
    /// Match case exactly (the default).
    #[arg(short = 's', long, overrides_with_all = ["ignore_case", "smart_case"],
        help_heading = "Search")]
    case_sensitive: bool,
    /// Fold case unless a pattern has an uppercase letter.
    #[arg(short = 'S', long, overrides_with_all = ["ignore_case", "case_sensitive"],
        help_heading = "Search")]
    smart_case: bool,
    /// Only match whole words.
    #[arg(short = 'w', long, help_heading = "Search")]
    word_regexp: bool,
    /// Only select lines that a pattern matches in full.
    #[arg(short = 'x', long, help_heading = "Search")]
    line_regexp: bool,
    /// Select the lines that do not match.
    #[arg(short = 'v', long, help_heading = "Search")]
    invert_match: bool,
    /// Stop each file after NUM selected lines.
    #[arg(short = 'm', long, value_name = "NUM", help_heading = "Search")]
    max_count: Option<usize>,
    /// Search binary files as if they were text.
    #[arg(short = 'a', long, help_heading = "Search")]
    text: bool,
    /// Search binary files found in directories, printing a notice instead of their lines.
    #[arg(long, help_heading = "Search")]
    binary: bool,
    /// Number of threads to search with [default: all cores].
    #[arg(short = 'j', long, value_name = "NUM", help_heading = "Search")]
    threads: Option<NonZeroUsize>,

    /// Also match text within N insertions, deletions, or substitutions.
    #[arg(short = 'k', long, value_name = "N", help_heading = "Extended search")]
    max_errors: Option<usize>,
    /// Treat each pattern as a boolean query, e.g. 'ERROR AND status=500 AND NOT ignored'.
    #[arg(long, help_heading = "Extended search")]
    query: bool,
    /// Report overlapping matches.
    #[arg(long, help_heading = "Extended search")]
    overlapping: bool,
    /// Parse files as JSON (or JSON Lines), YAML, or TOML: patterns match paths and values, or
    /// are predicates such as '$.latency_ms > 500'. Selected matches print as PATH:VALUE, and
    /// directories are searched for files of the format only. Without it, a pattern starting
    /// with '$.' or '$[' parses each file in the format its extension names.
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT",
        help_heading = "Extended search"
    )]
    parse: Option<ParseFormat>,
    /// Fail on nulls and type mismatches in predicates instead of skipping them.
    #[arg(long, help_heading = "Extended search")]
    strict: bool,

    /// Search hidden files and directories.
    #[arg(short = '.', long, help_heading = "Filter")]
    hidden: bool,
    /// Don't respect ignore files (.gitignore, .ignore, .rgignore, ...).
    #[arg(long, help_heading = "Filter")]
    no_ignore: bool,
    /// Reduce filtering: once for --no-ignore, twice to add --hidden, three times to add
    /// --binary.
    #[arg(short = 'u', long = "unrestricted", action = ArgAction::Count, help_heading = "Filter")]
    unrestricted: u8,
    /// Follow symbolic links.
    #[arg(short = 'L', long, help_heading = "Filter")]
    follow: bool,
    /// Descend at most NUM directories below each path.
    #[arg(short = 'd', long, value_name = "NUM", help_heading = "Filter")]
    max_depth: Option<usize>,
    /// Include or, with a leading '!', exclude files matching GLOB. Repeatable.
    #[arg(short = 'g', long, value_name = "GLOB", help_heading = "Filter")]
    glob: Vec<String>,
    /// Only search files of TYPE, such as rust or py. Repeatable.
    #[arg(
        short = 't',
        long = "type",
        value_name = "TYPE",
        help_heading = "Filter"
    )]
    types: Vec<String>,
    /// Don't search files of TYPE. Repeatable.
    #[arg(
        short = 'T',
        long = "type-not",
        value_name = "TYPE",
        help_heading = "Filter"
    )]
    types_not: Vec<String>,

    /// Print the number of selected lines per file, or of matches with -o. The last of -c,
    /// --count-matches, -l, and --files-without-match applies.
    #[arg(short = 'c', long, overrides_with_all = SUMMARIES, help_heading = "Output modes")]
    count: bool,
    /// Print the number of matches per file.
    #[arg(long, overrides_with_all = SUMMARIES, help_heading = "Output modes")]
    count_matches: bool,
    /// Print counts of zero too.
    #[arg(long, help_heading = "Output modes")]
    include_zero: bool,
    /// Print only the paths of files with a selected line.
    #[arg(short = 'l', long, overrides_with_all = SUMMARIES, help_heading = "Output modes")]
    files_with_matches: bool,
    /// Print only the paths of files without a selected line.
    #[arg(long, overrides_with_all = SUMMARIES, help_heading = "Output modes")]
    files_without_match: bool,
    /// Print each match on its own line instead of the whole line.
    #[arg(short = 'o', long, help_heading = "Output modes")]
    only_matching: bool,
    /// Print nothing, over any other output mode; exit 0 as soon as a line is selected.
    #[arg(short = 'q', long, help_heading = "Output modes")]
    quiet: bool,
    /// Print summary tables instead of matches. Sections: files, dirs, patterns, compare,
    /// cooccur, and with --parse paths and values=<JQ_PATH>. Files are chosen as by the
    /// library: hidden files are searched and only .gitignore files apply.
    #[arg(
        long,
        conflicts_with_all = [SUMMARIES.as_slice(), &["only_matching", "quiet"]].concat(),
        value_name = "SECTIONS",
        num_args = 0..=1,
        require_equals = true,
        value_delimiter = ',',
        default_missing_values = ["files", "patterns"],
        help_heading = "Output modes"
    )]
    report: Option<Vec<ReportSection>>,
    /// Rows per report table.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 10,
        requires = "report",
        help_heading = "Output modes"
    )]
    top: usize,

    /// Show line numbers (default on a terminal).
    #[arg(
        short = 'n',
        long,
        overrides_with = "no_line_number",
        help_heading = "Output"
    )]
    line_number: bool,
    /// Hide line numbers.
    #[arg(
        short = 'N',
        long,
        overrides_with = "line_number",
        help_heading = "Output"
    )]
    no_line_number: bool,
    /// Show the column of each line's first match, counted in bytes from 1. Turns on line
    /// numbers unless -N is given.
    #[arg(long, overrides_with = "no_column", help_heading = "Output")]
    column: bool,
    /// Hide columns.
    #[arg(long, overrides_with = "column", help_heading = "Output")]
    no_column: bool,
    /// End each printed path with a NUL byte instead of a separator or newline, for
    /// `xargs -0`.
    #[arg(short = '0', long, help_heading = "Output")]
    null: bool,
    /// Print the path of each match (default when searching a directory or several paths).
    #[arg(
        short = 'H',
        long,
        overrides_with = "no_filename",
        help_heading = "Output"
    )]
    with_filename: bool,
    /// Never print paths.
    #[arg(
        short = 'I',
        long,
        overrides_with = "with_filename",
        help_heading = "Output"
    )]
    no_filename: bool,
    /// Print each path once above its matches (default on a terminal).
    #[arg(long, overrides_with = "no_heading", help_heading = "Output")]
    heading: bool,
    /// Print the path on every line.
    #[arg(long, overrides_with = "heading", help_heading = "Output")]
    no_heading: bool,
    /// Lay output out for reading in a terminal: paths head their matches, a gutter shows
    /// which pattern matched, and a summary closes the search.
    #[arg(short = 'p', long, help_heading = "Output")]
    pretty: bool,
    /// When to color output.
    #[arg(long, value_enum, value_name = "WHEN", default_value_t = ColorChoice::Auto,
        help_heading = "Output")]
    color: ColorChoice,
    /// Show NUM lines after each selected line.
    #[arg(short = 'A', long, value_name = "NUM", help_heading = "Output")]
    after_context: Option<usize>,
    /// Show NUM lines before each selected line.
    #[arg(short = 'B', long, value_name = "NUM", help_heading = "Output")]
    before_context: Option<usize>,
    /// Show NUM lines before and after each selected line.
    #[arg(short = 'C', long, value_name = "NUM", help_heading = "Output")]
    context: Option<usize>,
    /// Order files found in directories [default: recent with --pretty, else none]. `recent`
    /// prints once the search ends; `path` searches with one thread.
    #[arg(long, value_enum, value_name = "SORTBY", help_heading = "Output")]
    sort: Option<SortBy>,
}

/// Structured formats `--parse` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ParseFormat {
    /// JSON documents.
    Json,
    /// YAML documents.
    Yaml,
    /// TOML documents.
    Toml,
}

/// When output is colored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    /// Never.
    Never,
    /// When printing to a terminal.
    Auto,
    /// Always.
    Always,
    /// Always, with ANSI escapes.
    Ansi,
}

/// Orders of the files found in directories; files named on the command line keep their
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SortBy {
    /// The most recently changed file first, by its last commit when git tracks it or else
    /// its modification time, to the minute, then by path.
    Recent,
    /// Lexicographic by path.
    Path,
    /// Whatever order parallel search finishes in.
    None,
}

/// What each searched file contributes to the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Selected lines, with any context.
    Lines,
    /// Each match on its own line.
    OnlyMatching,
    /// The number of selected lines.
    Count,
    /// The number of matches.
    CountMatches,
    /// The path, if a line is selected.
    FilesWithMatches,
    /// The path, if no line is selected.
    FilesWithoutMatch,
    /// Nothing.
    Quiet,
}

/// A table printed by `--report`, after the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportSection {
    /// Files with the most matches.
    Files,
    /// Directories with the most matches.
    Dirs,
    /// Match counts per pattern.
    Patterns,
    /// Per-pattern match counts of the top files.
    Compare,
    /// How often patterns match in the same file.
    Cooccur,
    /// Structured paths with the most matches.
    Paths,
    /// Most frequent values at a structured path.
    Values(String),
}

impl FromStr for ReportSection {
    type Err = String;

    fn from_str(section: &str) -> Result<Self, Self::Err> {
        Ok(match section {
            "files" => Self::Files,
            "dirs" => Self::Dirs,
            "patterns" => Self::Patterns,
            "compare" => Self::Compare,
            "cooccur" => Self::Cooccur,
            "paths" => Self::Paths,
            _ => match section.strip_prefix("values=") {
                Some(path) if !path.is_empty() => Self::Values(path.to_owned()),
                _ => {
                    return Err(format!(
                        "unknown section `{section}`; expected files, dirs, patterns, compare, \
                         cooccur, paths, or values=<JQ_PATH>"
                    ));
                }
            },
        })
    }
}

/// How selected lines are printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Printing {
    /// What each file contributes.
    pub mode: Mode,
    /// Print counts of zero.
    pub include_zero: bool,
    /// Print paths; `None` decides by whether a directory or several paths are searched.
    pub with_filename: Option<bool>,
    /// Prefix lines with their line number.
    pub line_number: bool,
    /// Prefix selected lines with the column of their first match.
    pub column: bool,
    /// End paths with a NUL byte.
    pub null: bool,
    /// Print each path once above its lines.
    pub heading: bool,
    /// Color paths, line numbers, and matches.
    pub color: bool,
    /// Context lines before each selected line.
    pub before: usize,
    /// Context lines after each selected line.
    pub after: usize,
}

/// Which lines of a file are selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Select lines without a match.
    pub invert: bool,
    /// Only lines that one match spans in full count as matching.
    pub whole_line: bool,
    /// Most lines selected per file.
    pub max_count: Option<usize>,
    /// Print the lines of files with NUL bytes instead of a notice.
    pub text: bool,
    /// Search files with NUL bytes found in directories, which are otherwise skipped.
    pub binary: bool,
}

impl Args {
    /// Positional arguments, patterns given with `-e`, and pattern files, in that order.
    pub fn pattern_sources(&self) -> (&[OsString], &[String], &[PathBuf]) {
        (&self.positionals, &self.patterns, &self.pattern_files)
    }

    /// Whether patterns are searched as written rather than parsed as regular expressions.
    /// Boolean queries and structured patterns have syntaxes of their own.
    pub fn literal_patterns(&self) -> bool {
        self.fixed_strings || self.query || self.parse.is_some()
    }

    /// Whether each file's structured format comes from its extension: without `--parse`,
    /// `-F`, or `--query`, a pattern starting with `$.` or `$[` is a structured pattern.
    pub fn infers_format(&self, patterns: &[String]) -> bool {
        self.parse.is_none()
            && !self.fixed_strings
            && !self.query
            && patterns
                .iter()
                .any(|pattern| pattern.starts_with("$.") || pattern.starts_with("$["))
    }

    /// The case mode for `patterns`, applying smart case.
    pub fn case_mode(&self, patterns: &[String]) -> CaseMode {
        let fold = self.ignore_case
            || (self.smart_case
                && !patterns
                    .iter()
                    .any(|pattern| pattern.chars().any(char::is_uppercase)));
        if fold {
            CaseMode::AsciiInsensitive
        } else {
            CaseMode::Sensitive
        }
    }

    /// Whole-word matching.
    pub fn word(&self) -> bool {
        self.word_regexp
    }

    /// The approximate-match edit distance, if any.
    pub fn max_errors(&self) -> Option<usize> {
        self.max_errors
    }

    /// Boolean query syntax.
    pub fn query(&self) -> bool {
        self.query
    }

    /// Overlapping matches.
    pub fn overlapping(&self) -> bool {
        self.overlapping
    }

    /// The structured format files are parsed as, if any.
    pub fn structured_mode(&self) -> Option<StructuredMode> {
        self.parse.map(|format| match format {
            ParseFormat::Json => StructuredMode::Json,
            ParseFormat::Yaml => StructuredMode::Yaml,
            ParseFormat::Toml => StructuredMode::Toml,
        })
    }

    /// How structured predicates treat nulls and type mismatches.
    pub fn predicate_handling(&self) -> StructuredPredicateHandling {
        if self.strict {
            StructuredPredicateHandling::Error
        } else {
            StructuredPredicateHandling::Ignore
        }
    }

    /// Which lines of a file are selected.
    pub fn selection(&self) -> Selection {
        Selection {
            invert: self.invert_match,
            whole_line: self.line_regexp,
            max_count: self.max_count,
            text: self.text,
            binary: self.binary || self.unrestricted >= 3,
        }
    }

    /// How selected lines are printed. Line numbers, headings, and color default on when
    /// stdout is a terminal.
    pub fn printing(&self) -> Printing {
        let terminal = io::stdout().is_terminal();
        let mode = if self.quiet {
            Mode::Quiet
        } else if self.count_matches || (self.count && self.only_matching) {
            Mode::CountMatches
        } else if self.count {
            Mode::Count
        } else if self.files_with_matches {
            Mode::FilesWithMatches
        } else if self.files_without_match {
            Mode::FilesWithoutMatch
        } else if self.only_matching {
            Mode::OnlyMatching
        } else {
            Mode::Lines
        };
        Printing {
            mode,
            include_zero: self.include_zero,
            with_filename: if self.with_filename {
                Some(true)
            } else if self.no_filename {
                Some(false)
            } else {
                None
            },
            line_number: if self.line_number {
                true
            } else {
                !self.no_line_number && (terminal || self.column)
            },
            column: self.column,
            null: self.null,
            heading: if self.heading {
                true
            } else {
                !self.no_heading && terminal
            },
            color: match self.color {
                ColorChoice::Never => false,
                ColorChoice::Auto => style::auto_color(terminal),
                ColorChoice::Always | ColorChoice::Ansi => true,
            },
            before: self.before_context.or(self.context).unwrap_or(0),
            after: self.after_context.or(self.context).unwrap_or(0),
        }
    }

    /// Whether output uses the `--pretty` layout.
    pub fn pretty(&self) -> bool {
        self.pretty
    }

    /// When to color output.
    pub fn color(&self) -> ColorChoice {
        self.color
    }

    /// Whether hidden files are searched.
    pub fn hidden(&self) -> bool {
        self.hidden || self.unrestricted >= 2
    }

    /// Whether ignore files are applied.
    pub fn respect_ignore_files(&self) -> bool {
        !self.no_ignore && self.unrestricted == 0
    }

    /// Whether symbolic links are followed.
    pub fn follow(&self) -> bool {
        self.follow
    }

    /// The deepest directory level searched below each path.
    pub fn max_depth(&self) -> Option<usize> {
        self.max_depth
    }

    /// Globs that include or, with a leading `!`, exclude files.
    pub fn globs(&self) -> &[String] {
        &self.glob
    }

    /// File types searched and file types skipped.
    pub fn file_types(&self) -> (&[String], &[String]) {
        (&self.types, &self.types_not)
    }

    /// The order of files found in directories: the one `--sort` names, else the most
    /// recently changed first for `--pretty`, whose headings date each file anyway, and
    /// ripgrep's finish order otherwise, which costs nothing.
    pub fn sort(&self) -> SortBy {
        self.sort.unwrap_or(if self.pretty {
            SortBy::Recent
        } else {
            SortBy::None
        })
    }

    /// The worker thread count, if set.
    pub fn threads(&self) -> Option<NonZeroUsize> {
        self.threads
    }

    /// The report sections requested, if `--report` was given.
    pub fn report(&self) -> Option<&[ReportSection]> {
        self.report.as_deref()
    }

    /// Rows per report table.
    pub fn top(&self) -> usize {
        self.top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("byteseek").chain(arguments.iter().copied()))
            .expect("arguments should parse")
    }

    #[test]
    fn long_help_ends_with_examples_and_short_help_does_not() {
        let mut command = <Args as clap::CommandFactory>::command();

        let long = command.render_long_help().to_string();
        let short = command.render_help().to_string();

        assert!(long.contains("Examples:\n  byteseek ERROR "));
        assert!(long.contains("Usage: byteseek [OPTIONS] PATTERN [PATH...]"));
        assert!(short.contains("[PATTERN] [PATH]..."));
        assert!(!short.contains("Examples:"));
    }

    #[test]
    fn last_case_flag_wins_and_smart_case_folds_lowercase_only() {
        let lowercase = ["error".to_owned()];
        let mixed = ["Error".to_owned()];

        assert_eq!(
            parse(&["-s", "-i"]).case_mode(&mixed),
            CaseMode::AsciiInsensitive
        );
        assert_eq!(
            parse(&["-i", "-s"]).case_mode(&lowercase),
            CaseMode::Sensitive
        );
        assert_eq!(
            parse(&["-S"]).case_mode(&lowercase),
            CaseMode::AsciiInsensitive
        );
        assert_eq!(parse(&["-S"]).case_mode(&mixed), CaseMode::Sensitive);
    }

    #[test]
    fn output_modes_combine_as_in_ripgrep() {
        let mode = |arguments: &[&str]| parse(arguments).printing().mode;

        assert_eq!(mode(&["-c", "x"]), Mode::Count);
        assert_eq!(mode(&["--count-matches", "x"]), Mode::CountMatches);
        assert_eq!(mode(&["-c", "-l", "x"]), Mode::FilesWithMatches);
        assert_eq!(mode(&["-l", "-c", "x"]), Mode::Count);
        assert_eq!(
            mode(&["-l", "--files-without-match", "x"]),
            Mode::FilesWithoutMatch
        );
        assert_eq!(mode(&["-o", "-c", "x"]), Mode::CountMatches);
        assert_eq!(mode(&["-o", "-l", "x"]), Mode::FilesWithMatches);
        assert_eq!(mode(&["-c", "-q", "-o", "x"]), Mode::Quiet);
        assert_eq!(mode(&["-o", "-o", "-n", "-n", "x"]), Mode::OnlyMatching);
        assert!(
            parse(&["-c", "--include-zero", "x"])
                .printing()
                .include_zero
        );
        assert!(Args::try_parse_from(["byteseek", "--report", "-o", "x"]).is_err());
        assert!(Args::try_parse_from(["byteseek", "-c", "--report", "x"]).is_err());
    }

    #[test]
    fn explicit_output_flags_override_terminal_defaults() {
        let printing = parse(&[
            "-n",
            "-H",
            "--heading",
            "--color",
            "always",
            "-C",
            "2",
            "-A",
            "1",
        ])
        .printing();

        assert!(printing.line_number && printing.heading && printing.color);
        assert_eq!(printing.with_filename, Some(true));
        assert_eq!((printing.before, printing.after), (2, 1));
        assert!(!parse(&["-n", "-N"]).printing().line_number);
        assert_eq!(parse(&["-H", "-I"]).printing().with_filename, Some(false));
    }

    #[test]
    fn pretty_files_are_ordered_by_last_change_unless_another_sort_is_named() {
        assert_eq!(parse(&["x"]).sort(), SortBy::None);
        assert_eq!(parse(&["-p", "x"]).sort(), SortBy::Recent);
        assert_eq!(parse(&["--sort", "recent", "x"]).sort(), SortBy::Recent);
        assert_eq!(parse(&["-p", "--sort", "none", "x"]).sort(), SortBy::None);
        assert_eq!(parse(&["--sort", "path", "x"]).sort(), SortBy::Path);
        assert_eq!(parse(&["--sort", "none", "x"]).sort(), SortBy::None);
    }

    #[test]
    fn column_turns_on_line_numbers_unless_hidden() {
        let column = parse(&["--column", "x"]).printing();
        let hidden = parse(&["--column", "-N", "x"]).printing();

        assert!(column.column && column.line_number);
        assert!(hidden.column && !hidden.line_number);
        assert!(!parse(&["--column", "--no-column", "x"]).printing().column);
        assert!(parse(&["-0", "x"]).printing().null);
    }

    #[test]
    fn unrestricted_flags_lift_filters_in_steps() {
        let once = parse(&["-u"]);
        let thrice = parse(&["-uuu"]);

        assert!(!once.respect_ignore_files() && !once.hidden() && !once.selection().binary);
        assert!(thrice.hidden() && thrice.selection().binary && !thrice.selection().text);
        assert!(parse(&[]).respect_ignore_files());
    }

    #[test]
    fn structured_options_set_the_format_and_predicate_handling() {
        assert_eq!(
            parse(&["--parse", "yaml", "--strict"]).structured_mode(),
            Some(StructuredMode::Yaml)
        );
        assert!(parse(&["--parse", "toml"]).literal_patterns());
        assert_eq!(
            parse(&["--strict", "$.x > 1"]).predicate_handling(),
            StructuredPredicateHandling::Error
        );
    }

    #[test]
    fn report_sections_parse_with_a_default() {
        assert_eq!(
            parse(&["--report", "x"]).report(),
            Some([ReportSection::Files, ReportSection::Patterns].as_slice())
        );
        assert_eq!(
            parse(&["--report=dirs,values=$.status", "x"]).report(),
            Some(
                [
                    ReportSection::Dirs,
                    ReportSection::Values("$.status".to_owned())
                ]
                .as_slice()
            )
        );
        assert!(Args::try_parse_from(["byteseek", "--report=bogus", "x"]).is_err());
    }
}
