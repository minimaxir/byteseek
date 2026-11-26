//! The `byteseek` command line: literal, fuzzy, boolean, and structured search with ripgrep's
//! options, shared by the `byteseek` binary and the Python package's console script.

mod args;
mod changed;
mod diagnostics;
mod inputs;
mod pieces;
mod pretty;
mod printer;
mod report;
mod search;
mod style;
mod walk;

use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::cli::args::{Args, ColorChoice, Mode};
use crate::cli::diagnostics::Diagnostics;
use crate::cli::inputs::{Input, Inputs};
use crate::cli::pretty::{Pretty, Tally};
use crate::cli::printer::Printer;
use crate::cli::search::{FileSearcher, FormatSearchers};
use crate::cli::walk::Walker;
use crate::{CaseMode, SearchConfig, Searcher, StructuredPredicateHandling};

/// Exit status when a line was selected, or for `--help` and `--version`.
const SUCCESS: u8 = 0;
/// Exit status when nothing was selected.
const NO_MATCH: u8 = 1;
/// Exit status after an error.
const ERROR: u8 = 2;

/// Runs the `byteseek` command line, writing results to standard output and diagnostics to
/// standard error.
///
/// Unlike a process's `main`, it never exits the process, so it can run inside another
/// program such as the Python interpreter. Settings that belong to the whole process stay
/// with the first call: the size of Rayon's global thread pool, which a later `--threads`
/// cannot change, and how diagnostics are formatted.
///
/// # Arguments
///
/// * `args` - The command line, starting with the program name, as `std::env::args_os`
///   gives it
///
/// # Returns
///
/// ripgrep's exit status: 0 if a line was selected, 1 if none was, and 2 after an error,
/// including invalid arguments. `--help` and `--version` return 0.
///
/// # Examples
///
/// ```no_run
/// let status = byteseek::cli::run(["byteseek", "--count", "error", "app.log"]);
/// std::process::exit(status.into());
/// ```
pub fn run<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    // A later call in the same process keeps the subscriber the first one installed.
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .event_format(Diagnostics {
            color: style::detect(false, false, io::stderr().is_terminal()) != style::Depth::None,
        })
        .try_init();
    let args = match Args::try_parse_from(args) {
        Ok(args) => args,
        Err(usage) => return usage_status(&usage),
    };
    // Rust only flushes standard output when a Rust `main` returns, so flush a last line
    // without a newline, such as `--null` output, before an embedding process exits.
    let outcome = search(&args).and_then(|status| {
        io::stdout().flush()?;
        Ok(status)
    });
    match outcome {
        Ok(status) => status,
        // A closed pipe, as in `byteseek ... | head`, ends the search normally.
        Err(error) if is_broken_pipe(&error) => SUCCESS,
        Err(error) => match error.downcast_ref::<clap::Error>() {
            Some(usage) => usage_status(usage),
            None => {
                tracing::error!("{error:#}");
                ERROR
            }
        },
    }
}

/// Prints a usage error, help, or version as `clap::Error::exit` does, without exiting.
fn usage_status(usage: &clap::Error) -> u8 {
    // Like `exit`, a closed output stream leaves only the status to report.
    let _ = usage.print();
    if usage.use_stderr() { ERROR } else { SUCCESS }
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|io_error| io_error.kind() == io::ErrorKind::BrokenPipe)
    })
}

fn search(args: &Args) -> Result<u8> {
    let started = Instant::now();
    if let Some(threads) = args.threads() {
        let mut pool = rayon::ThreadPoolBuilder::new().num_threads(threads.get());
        if threads.get() == 1 {
            // Searches then run inline on this thread instead of waking a worker.
            pool = pool.use_current_thread();
        }
        pool.build_global()
            .context("failed to start worker threads")?;
    }

    let inputs = Inputs::from_args(args)?;
    let case_mode = args.case_mode(&inputs.patterns);
    if case_mode == CaseMode::AsciiInsensitive && inputs.patterns.iter().any(|p| !p.is_ascii()) {
        tracing::warn!("case folding covers ASCII letters only");
    }
    let builder = SearchConfig::builder()
        .case_mode(case_mode)
        .whole_word(args.word())
        .max_edit_distance(args.max_errors())
        .boolean_queries(args.query())
        .overlapping(args.overlapping())
        .structured_mode(args.structured_mode())
        .structured_predicate_handling(args.predicate_handling());
    let searcher = Searcher::new(&inputs.patterns, builder.build()).context("invalid pattern")?;
    let formats = inputs
        .infers_format
        .then(|| {
            FormatSearchers::new(|mode| {
                Searcher::new(
                    &inputs.patterns,
                    builder.structured_mode(Some(mode)).build(),
                )
            })
        })
        .transpose()
        .context("invalid pattern")?;
    let structured = args.structured_mode().is_some() || formats.is_some();
    if !structured && args.predicate_handling() == StructuredPredicateHandling::Error {
        bail!("--strict needs structured patterns: start one with $. or $[, or pass --parse");
    }

    if let Some(sections) = args.report() {
        if formats.is_some() {
            bail!("--report needs --parse json, yaml, or toml for structured patterns");
        }
        let mut out = io::BufWriter::new(io::stdout().lock());
        let matched = report::print(&mut out, &searcher, &inputs.inputs, sections, args.top())?;
        out.flush()?;
        return Ok(exit_status(matched, false, Mode::Lines));
    }

    let mut printing = args.printing();
    let values = (args.pretty() && structured)
        .then(|| Searcher::new(&inputs.patterns, builder.structured_mode(None).build()).ok())
        .flatten();
    let pretty = args.pretty().then(|| Pretty {
        depth: style::detect(
            matches!(args.color(), ColorChoice::Always | ColorChoice::Ansi),
            args.color() == ColorChoice::Never,
            io::stdout().is_terminal(),
        ),
        width: io::stdout()
            .is_terminal()
            .then(terminal_size::terminal_size)
            .flatten()
            .map(|(width, _)| usize::from(width.0)),
        patterns: inputs.patterns.len(),
        values: values.as_ref(),
    });
    if pretty.is_some() {
        printing.with_filename = Some(true);
        printing.heading = true;
    }
    let tally = Tally::new(inputs.patterns.len());
    let searches_several = inputs.inputs.len() > 1
        || inputs
            .inputs
            .iter()
            .any(|input| matches!(input, Input::Dir(_)));
    let files = FileSearcher {
        searcher: &searcher,
        selection: args.selection(),
        mode: printing.mode,
        include_zero: printing.include_zero,
        context: (printing.before, printing.after),
        printer: Printer {
            with_filename: printing.with_filename.unwrap_or(searches_several),
            heading: printing.heading,
            line_number: printing.line_number,
            column: printing.column,
            null: printing.null,
            color: printing.color,
        },
        pretty,
        tally: pretty.is_some().then_some(&tally),
        structured,
        formats: formats.as_ref(),
        line_local: args.max_errors().is_none()
            && !inputs.patterns.iter().any(|pattern| pattern.contains('\n')),
    };
    let (matched, errored) = Walker::new(files, args, &inputs).run(&inputs.inputs)?;
    if let Some(pretty) = pretty.filter(|_| printing.mode != Mode::Quiet) {
        let mut summary = Vec::new();
        if matched {
            summary.push(b'\n');
        }
        pretty.summary(
            &mut summary,
            &tally,
            &inputs.patterns,
            started.elapsed(),
            structured,
        );
        io::stdout().lock().write_all(&summary)?;
    }
    Ok(exit_status(matched, errored, printing.mode))
}

/// ripgrep's exit status: 2 after an error unless `--quiet` found a match, else 0 if a line
/// was selected and 1 if none was.
fn exit_status(matched: bool, errored: bool, mode: Mode) -> u8 {
    if errored && !(mode == Mode::Quiet && matched) {
        ERROR
    } else if matched {
        SUCCESS
    } else {
        NO_MATCH
    }
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;
    use tempfile::NamedTempFile;

    use super::*;

    /// Runs `--quiet` so a selected line prints nothing, only setting the status.
    fn quiet_search(pattern: &str, text: &str) -> u8 {
        let mut file = NamedTempFile::new().expect("create a temporary file");
        file.write_all(text.as_bytes())
            .expect("write the temporary file");
        run([
            OsString::from("byteseek"),
            "--quiet".into(),
            pattern.into(),
            file.path().into(),
        ])
    }

    #[test]
    fn run_returns_statuses_instead_of_exiting() {
        assert_eq!(quiet_search("beta", "alpha\nbeta\n"), SUCCESS);
        assert_eq!(quiet_search("gamma", "alpha\nbeta\n"), NO_MATCH);
        assert_eq!(run(["byteseek", "--no-such-flag"]), ERROR);
        // The diagnostics subscriber from the first run stays installed.
        assert_eq!(quiet_search("alpha", "alpha\n"), SUCCESS);
    }

    #[test]
    fn usage_status_follows_where_clap_prints() {
        assert_eq!(
            usage_status(&clap::Error::new(ErrorKind::DisplayVersion)),
            SUCCESS
        );
        assert_eq!(
            usage_status(&clap::Error::new(ErrorKind::UnknownArgument)),
            ERROR
        );
    }

    #[test]
    fn broken_pipe_is_found_under_context() {
        let closed = anyhow::Error::from(io::Error::from(io::ErrorKind::BrokenPipe));
        assert!(is_broken_pipe(&closed.context("writing results")));
        let missing = anyhow::Error::from(io::Error::from(io::ErrorKind::NotFound));
        assert!(!is_broken_pipe(&missing));
    }

    #[test]
    fn exit_status_matches_ripgrep() {
        assert_eq!(exit_status(true, false, Mode::Lines), SUCCESS);
        assert_eq!(exit_status(false, false, Mode::Lines), NO_MATCH);
        assert_eq!(exit_status(true, true, Mode::Lines), ERROR);
        assert_eq!(exit_status(true, true, Mode::Quiet), SUCCESS);
        assert_eq!(exit_status(false, true, Mode::Quiet), ERROR);
    }
}
