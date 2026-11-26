//! Patterns and inputs from the command line.

use std::ffi::OsString;
use std::fs;
use std::io;
#[cfg(not(unix))]
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap::error::ErrorKind;
use regex_syntax::hir::HirKind;

use crate::cli::args::Args;

/// Something to search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Standard input.
    Stdin,
    /// A path that is not a directory. It is searched even if ignore rules or filters would
    /// skip it, and failures to read it surface when it is searched.
    File(PathBuf),
    /// A directory, searched recursively.
    Dir(PathBuf),
}

impl Input {
    fn new(path: OsString) -> Self {
        if path == "-" {
            return Self::Stdin;
        }
        let path = PathBuf::from(path);
        if fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            Self::Dir(path)
        } else {
            Self::File(path)
        }
    }
}

/// What to search for and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    /// Literal patterns, or byteseek queries and predicates.
    pub patterns: Vec<String>,
    /// Inputs in command-line order; never empty.
    pub inputs: Vec<Input>,
    /// No path was given, so `./` is searched and printed paths drop that prefix.
    pub implicit_dir: bool,
    /// Structured patterns without `--parse`, so each file is parsed as its extension says.
    pub infers_format: bool,
}

impl Inputs {
    /// Collects patterns from positionals, `-e`, and `-f`, then the inputs.
    ///
    /// # Arguments
    ///
    /// * `args` - Parsed command-line arguments.
    ///
    /// # Returns
    ///
    /// The patterns, converted from regular expressions to literals unless they are searched
    /// as written, and the inputs.
    ///
    /// # Errors
    ///
    /// Returns an error if no pattern is given, a positional pattern is not UTF-8, a pattern
    /// file cannot be read, or a pattern is a regular expression that matches more than one
    /// fixed string.
    pub fn from_args(args: &Args) -> Result<Self> {
        let (positionals, flagged, files) = args.pattern_sources();
        let mut patterns = flagged.to_vec();
        for path in files {
            let text = fs::read_to_string(path)
                .with_context(|| format!("{}: failed to read patterns", path.display()))?;
            patterns.extend(text.lines().map(str::to_owned));
        }

        let mut positionals = positionals.iter().cloned();
        if flagged.is_empty() && files.is_empty() {
            let pattern = positionals.next().ok_or_else(|| {
                usage_error(ErrorKind::MissingRequiredArgument, "no pattern given")
            })?;
            patterns.push(
                pattern.into_string().map_err(|_| {
                    usage_error(ErrorKind::InvalidUtf8, "pattern is not valid UTF-8")
                })?,
            );
        }
        let infers_format = args.infers_format(&patterns);
        if !args.literal_patterns() && !infers_format {
            patterns = patterns
                .iter()
                .map(|pattern| literal(pattern))
                .collect::<Result<_>>()?;
        }

        let mut inputs: Vec<Input> = positionals.map(Input::new).collect();
        let implicit_dir = inputs.is_empty() && !stdin_is_readable();
        if inputs.is_empty() {
            inputs.push(if implicit_dir {
                Input::Dir(PathBuf::from("./"))
            } else {
                Input::Stdin
            });
        }
        Ok(Self {
            patterns,
            inputs,
            implicit_dir,
            infers_format,
        })
    }
}

/// Whether stdin is a file, pipe, or socket, which ripgrep searches when no path is given.
/// A terminal, `/dev/null`, or a closed descriptor means the current directory is searched.
fn stdin_is_readable() -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        use std::os::unix::fs::FileTypeExt;

        io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .and_then(|descriptor| fs::File::from(descriptor).metadata())
            .is_ok_and(|metadata| {
                let file_type = metadata.file_type();
                file_type.is_file() || file_type.is_fifo() || file_type.is_socket()
            })
    }
    #[cfg(not(unix))]
    {
        !io::stdin().is_terminal()
    }
}

/// A misused command line, which `main` reports with clap's usage and hint.
fn usage_error(kind: ErrorKind, message: impl std::fmt::Display) -> anyhow::Error {
    Args::command().error(kind, message).into()
}

/// The one string a regular expression matches.
///
/// # Errors
///
/// Returns an error if `pattern` does not parse, or matches anything but one fixed string.
fn literal(pattern: &str) -> Result<String> {
    let hir = regex_syntax::parse(pattern)
        .with_context(|| format!("`{pattern}` is not a valid regular expression"))?;
    match hir.kind() {
        HirKind::Empty => Ok(String::new()),
        HirKind::Literal(literal) => Ok(String::from_utf8(literal.0.to_vec())
            .context("a literal in Unicode mode is always UTF-8")?),
        HirKind::Class(_)
        | HirKind::Look(_)
        | HirKind::Repetition(_)
        | HirKind::Capture(_)
        | HirKind::Concat(_)
        | HirKind::Alternation(_) => Err(usage_error(
            ErrorKind::ValueValidation,
            format!(
                "`{pattern}` is a regular expression; byteseek searches literal text, so pass -F \
                 to search for it as written"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn inputs(arguments: &[&str]) -> Result<Inputs> {
        let args =
            Args::try_parse_from(std::iter::once("byteseek").chain(arguments.iter().copied()))
                .expect("arguments should parse");
        Inputs::from_args(&args)
    }

    #[test]
    fn literal_regexes_unescape_and_others_are_rejected() {
        assert_eq!(literal("foo").expect("plain text is literal"), "foo");
        assert_eq!(
            literal(r"foo\.bar\(").expect("escapes are literal"),
            "foo.bar("
        );
        assert!(literal("foo.*bar").is_err());
        assert!(literal("a|b").is_err());
        assert!(literal("(unclosed").is_err());
    }

    #[test]
    fn first_positional_is_the_pattern_without_e() {
        let found = inputs(&["ERROR", "-"]).expect("arguments should split");

        assert_eq!(found.patterns, ["ERROR"]);
        assert_eq!(found.inputs, [Input::Stdin]);
        assert!(!found.implicit_dir);
    }

    #[test]
    fn every_positional_is_an_input_with_e() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let directory_arg = directory
            .path()
            .to_str()
            .expect("temporary path should be UTF-8");

        let found = inputs(&["-e", "a", "-e", "b", directory_arg, "missing.txt"])
            .expect("arguments should split");

        assert_eq!(found.patterns, ["a", "b"]);
        assert_eq!(
            found.inputs,
            [
                Input::Dir(directory.path().to_path_buf()),
                Input::File(PathBuf::from("missing.txt"))
            ]
        );
    }

    #[test]
    fn pattern_files_add_one_pattern_per_line() {
        let mut file = tempfile::NamedTempFile::new().expect("temporary file should be created");
        io::Write::write_all(&mut file, b"alpha\nbe.ta\n").expect("patterns should write");
        let file_arg = file
            .path()
            .to_str()
            .expect("temporary path should be UTF-8");

        assert!(inputs(&["-f", file_arg, "-"]).is_err());
        assert_eq!(
            inputs(&["-F", "-f", file_arg, "-"])
                .expect("fixed strings")
                .patterns,
            ["alpha", "be.ta"]
        );
    }

    #[test]
    fn regex_patterns_need_fixed_strings() {
        assert!(inputs(&["a+b", "-"]).is_err());
        assert_eq!(
            inputs(&["-F", "a+b", "-"]).expect("fixed").patterns,
            ["a+b"]
        );
        assert_eq!(
            inputs(&["--query", "a AND (b OR c)", "-"])
                .expect("queries")
                .patterns,
            ["a AND (b OR c)"]
        );
    }
}
