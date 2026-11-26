//! Searching stdin, named files, and directories, and printing each file's output whole:
//! named inputs in command-line order, then the files found in directories in `--sort` order.

use std::env;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use anyhow::{Context, Result};
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use ignore::{DirEntry, WalkBuilder, WalkState};
use memchr::memchr_iter;

use crate::StructuredMode;
use crate::cli::args::{Args, Mode, SortBy};
use crate::cli::changed::{LastChange, last_changes};
use crate::cli::inputs::{Input, Inputs};
use crate::cli::pieces::{CONCURRENT_PIECE_LEN, PIECE_LEN};
use crate::cli::pretty::Pretty;
use crate::cli::search::{Binary, Emit, FileSearcher, Source, WriteError, format_globs};

/// The name printed for standard input.
pub const STDIN_NAME: &[u8] = b"<stdin>";
/// Files whose output may wait in the channel while the main thread prints.
const PENDING_FILES: usize = 256;

/// How a search ended, for the exit status.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Some file counted as a success.
    pub matched: AtomicBool,
    /// Some file or path could not be searched.
    pub errored: AtomicBool,
}

/// Searches inputs and prints their output.
pub struct Walker<'a> {
    files: FileSearcher<'a>,
    args: &'a Args,
    implicit_dir: bool,
    outcome: Outcome,
}

/// The file type that selects the files of the structured formats being searched.
const STRUCTURED_TYPE: &str = "structured";

/// A file's output, with the file's path when its last change orders the output or ends a
/// `--pretty` heading.
#[derive(Debug, Default)]
struct FileOutput {
    bytes: Vec<u8>,
    path: Option<PathBuf>,
    /// The output starts with a `--pretty` heading, which shows when the file last changed.
    headed: bool,
}

impl FileOutput {
    /// Ends the heading, the output's first line, with when the file last changed. The path
    /// in the heading is printed as is, so its own line breaks are skipped.
    fn add_last_change(&mut self, pretty: &Pretty<'_>, change: LastChange) {
        let Some(path) = self.path.as_ref().filter(|_| self.headed) else {
            return;
        };
        let breaks_in_path = memchr_iter(b'\n', path.as_os_str().as_encoded_bytes()).count();
        let Some(heading_end) = memchr_iter(b'\n', &self.bytes).nth(breaks_in_path) else {
            return;
        };
        let mut detail = Vec::new();
        pretty.last_change(&mut detail, change);
        self.bytes.splice(heading_end..heading_end, detail);
    }
}

/// Writes whole files to stdout, separated as ripgrep separates them.
struct Sink<'a, W> {
    out: W,
    /// Written between files: a blank line in heading mode, or `--` between lines of context.
    file_separator: &'static [u8],
    wrote_file: bool,
    /// The `--pretty` layout, whose headings show when files last changed.
    pretty: Option<Pretty<'a>>,
}

impl<W: Write> Sink<'_, W> {
    /// Writes `outputs`, dating their `--pretty` headings with one lookup for all of them.
    /// With `newest_first`, the most recently changed file comes first, to the minute that
    /// headings show, then files by path; files whose last change can't be read come last.
    fn files(&mut self, outputs: Vec<FileOutput>, newest_first: bool) -> io::Result<()> {
        let dates_headings = self.pretty.is_some();
        if !newest_first && !dates_headings {
            return outputs
                .iter()
                .try_for_each(|output| self.file(&output.bytes));
        }
        let looked_up: Vec<usize> = outputs
            .iter()
            .enumerate()
            .filter(|(_, output)| {
                output.path.is_some() && (newest_first || (dates_headings && output.headed))
            })
            .map(|(index, _)| index)
            .collect();
        let paths: Vec<&Path> = looked_up
            .iter()
            .filter_map(|&index| outputs[index].path.as_deref())
            .collect();
        let changes = last_changes(&paths);
        let mut keyed: Vec<(Option<i64>, FileOutput)> =
            outputs.into_iter().map(|output| (None, output)).collect();
        for (index, change) in looked_up.into_iter().zip(changes) {
            let Some(change) = change else {
                continue;
            };
            let (time, output) = &mut keyed[index];
            *time = Some(change.minute());
            if let Some(pretty) = &self.pretty {
                output.add_last_change(pretty, change);
            }
        }
        if newest_first {
            keyed.sort_unstable_by(|(left_time, left), (right_time, right)| {
                right_time
                    .cmp(left_time)
                    .then_with(|| left.path.cmp(&right.path))
            });
        }
        keyed
            .iter()
            .try_for_each(|(_, output)| self.file(&output.bytes))
    }

    fn file(&mut self, output: &[u8]) -> io::Result<()> {
        self.file_part(output, &mut false)
    }

    /// Writes part of a file's output. `started` says whether the file wrote any before, so
    /// the blank line between files comes before its first part.
    fn file_part(&mut self, output: &[u8], started: &mut bool) -> io::Result<()> {
        if output.is_empty() {
            return Ok(());
        }
        if !*started {
            *started = true;
            if self.wrote_file {
                self.out.write_all(self.file_separator)?;
            }
            self.wrote_file = true;
        }
        self.out.write_all(output)
    }

    /// Writes files as they arrive until the walk ends, taking every file already waiting as
    /// one batch when headings are dated. Returning drops `receiver`, which stops the walk on
    /// a write error.
    fn drain(&mut self, receiver: Receiver<FileOutput>) -> io::Result<()> {
        if self.pretty.is_none() {
            return receiver
                .into_iter()
                .try_for_each(|output| self.file(&output.bytes));
        }
        while let Ok(first) = receiver.recv() {
            let mut batch = vec![first];
            batch.extend(receiver.try_iter().take(PENDING_FILES));
            self.files(batch, false)?;
        }
        Ok(())
    }
}

impl<'a> Walker<'a> {
    /// Creates a walker for the inputs `inputs` describes.
    pub fn new(files: FileSearcher<'a>, args: &'a Args, inputs: &Inputs) -> Self {
        Self {
            files,
            args,
            implicit_dir: inputs.implicit_dir,
            outcome: Outcome::default(),
        }
    }

    /// Searches every input in order, printing as files finish.
    ///
    /// # Returns
    ///
    /// Whether a file counted as a success, and whether any input failed.
    ///
    /// # Errors
    ///
    /// Returns an error if stdout cannot be written or the filters are invalid. Failures to
    /// read inputs are logged and recorded in the outcome instead.
    pub fn run(self, inputs: &[Input]) -> Result<(bool, bool)> {
        let mut sink = Sink {
            out: io::BufWriter::new(io::stdout()),
            file_separator: self.file_separator(),
            wrote_file: false,
            pretty: self.files.pretty.filter(|_| self.dates_headings()),
        };
        let directories: Vec<&Path> = inputs
            .iter()
            .filter_map(|input| match input {
                Input::Dir(path) => Some(path.as_path()),
                Input::Stdin | Input::File(_) => None,
            })
            .collect();
        for input in inputs {
            if self.done() {
                break;
            }
            // Output a search can't write as it goes, such as `--pretty` output, comes back whole.
            let mut started = false;
            let mut emit = |bytes: &[u8]| sink.file_part(bytes, &mut started);
            let output = match input {
                Input::Stdin => self.stdin(Some(&mut emit))?,
                Input::File(path) => self.named_file(path, Some(&mut emit))?,
                Input::Dir(_) => continue,
            };
            if !started {
                sink.files(vec![output], false)?;
            }
        }
        if !directories.is_empty() && !self.done() {
            self.directories(&directories, &mut sink)?;
        }
        sink.out.flush()?;
        Ok((
            self.outcome.matched.into_inner(),
            self.outcome.errored.into_inner(),
        ))
    }

    /// What separates files' output, as in ripgrep: a blank line between files of lines or
    /// matches in heading mode, even without paths, else `--` when lines print with context.
    fn file_separator(&self) -> &'static [u8] {
        let (files, printer) = (&self.files, self.files.printer);
        if !files.prints_lines() {
            b""
        } else if printer.heading {
            b"\n"
        } else if files.context != (0, 0) && files.pretty.is_none() {
            b"--\n"
        } else {
            b""
        }
    }

    /// Whether `--pretty` headings head each file's lines or matches, and show when the file
    /// last changed.
    fn dates_headings(&self) -> bool {
        self.files.pretty.is_some() && matches!(self.files.mode, Mode::Lines | Mode::OnlyMatching)
    }

    /// Whether `--quiet` has found what it needs.
    fn done(&self) -> bool {
        self.files.mode == Mode::Quiet && self.outcome.matched.load(Ordering::Relaxed)
    }

    fn failed(&self) {
        self.outcome.errored.store(true, Ordering::Relaxed);
        if let Some(tally) = self.files.tally {
            tally.failed();
        }
    }

    /// Records how searching an input ended. A failed write ends the whole search, so it is
    /// returned instead.
    fn record(&self, path: &[u8], result: Result<bool>) -> Result<()> {
        match result {
            Ok(true) => self.outcome.matched.store(true, Ordering::Relaxed),
            Ok(false) => {}
            Err(error) if error.is::<WriteError>() => return Err(error),
            Err(error) => {
                tracing::error!("{}: {error:#}", String::from_utf8_lossy(path));
                self.failed();
            }
        }
        Ok(())
    }

    /// Searches standard input, writing output to `emit` as it is found when given.
    fn stdin(&self, emit: Option<Emit<'_>>) -> Result<FileOutput> {
        let mut output = FileOutput::default();
        let source = Source {
            name: STDIN_NAME,
            reader: io::stdin().lock(),
            len: None,
            path: None,
            binary: self.explicit_binary(),
            piece_len: PIECE_LEN,
            parallel: false,
        };
        let result = self.search_input(source, &mut output, emit);
        self.record(STDIN_NAME, result)?;
        Ok(output)
    }

    /// Searches a file named on the command line, whatever the filters say, writing output to
    /// `emit` as it is found when given.
    fn named_file(&self, path: &Path, emit: Option<Emit<'_>>) -> Result<FileOutput> {
        let name = path.as_os_str().as_encoded_bytes();
        let mut output = FileOutput::default();
        let result = File::open(path)
            .map_err(anyhow::Error::from)
            .and_then(|file| {
                let len = regular_len(&file);
                let source = Source {
                    name,
                    reader: file,
                    len,
                    path: len.map(|_| path),
                    binary: self.explicit_binary(),
                    piece_len: PIECE_LEN,
                    parallel: true,
                };
                self.search_input(source, &mut output, emit)
            });
        if output.headed {
            output.path = Some(path.to_path_buf());
        }
        self.record(name, result)?;
        Ok(output)
    }

    /// What a NUL byte does to an input the user named, as in ripgrep: without `--text`, lines
    /// print up to the first one's line, then a notice if any line matched. Counts and paths
    /// search NUL bytes as they are.
    fn explicit_binary(&self) -> Binary {
        if self.files.prints_lines() && !self.files.selection.text {
            Binary::Notice
        } else {
            Binary::Search
        }
    }

    /// What a NUL byte does to a file found by walking a directory, as in ripgrep: the search
    /// stops at its line unless `--text` or `--binary` is given.
    fn walked_binary(&self) -> Binary {
        let selection = self.files.selection;
        if selection.text || selection.binary {
            self.explicit_binary()
        } else {
            Binary::Quit
        }
    }

    /// Searches an input a piece of whole lines at a time when its output allows it, else
    /// whole. Output goes to `emit` as it is found when both it and the search allow it.
    fn search_input<R: Read>(
        &self,
        source: Source<'_, R>,
        output: &mut FileOutput,
        emit: Option<Emit<'_>>,
    ) -> Result<bool> {
        let streams = self.files.streams();
        if !source.binary.stops()
            && self.files.counts_by_stream()
            && !(streams && source.reads_in_parallel())
        {
            return (self.files).count_stream(source.name, source.reader, &mut output.bytes);
        }
        if streams {
            return self.files.search_reader(source, &mut output.bytes, emit);
        }
        let Source {
            name,
            mut reader,
            len,
            binary,
            ..
        } = source;
        let mut bytes = Vec::with_capacity(len.map_or(0, |len| usize::try_from(len).unwrap_or(0)));
        reader.read_to_end(&mut bytes)?;
        let (matched, headed) = (self.files).search(name, &mut bytes, binary, &mut output.bytes)?;
        output.headed = self.dates_headings() && headed;
        Ok(matched)
    }

    /// Searches a file found by walking a directory, in pieces of at most `piece_len` bytes,
    /// on every thread when `parallel`, writing output to `emit` as it is found when given.
    fn walked_file(
        &self,
        entry: Result<DirEntry, ignore::Error>,
        emit: Option<Emit<'_>>,
        (piece_len, parallel): (usize, bool),
    ) -> Result<FileOutput> {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::error!("{error}");
                self.failed();
                return Ok(FileOutput::default());
            }
        };
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            return Ok(FileOutput::default());
        }
        let path = entry.path();
        let shown = if self.implicit_dir {
            path.strip_prefix("./").unwrap_or(path)
        } else {
            path
        };
        let name = shown.as_os_str().as_encoded_bytes();
        let mut output = FileOutput::default();
        let result = File::open(path)
            .map_err(anyhow::Error::from)
            .and_then(|file| {
                let len = regular_len(&file);
                let source = Source {
                    name,
                    reader: file,
                    len,
                    path: len.map(|_| path),
                    binary: self.walked_binary(),
                    piece_len,
                    parallel,
                };
                self.search_input(source, &mut output, emit)
            });
        let ordered = self.args.sort() == SortBy::Recent && !output.bytes.is_empty();
        if output.headed || ordered {
            output.path = Some(shown.to_path_buf());
        }
        self.record(name, result)?;
        Ok(output)
    }

    /// The formats structured search parses: the one `--parse` names, or all of them when
    /// each file's extension names its own. Empty for text search.
    fn structured_formats(&self) -> &'static [StructuredMode] {
        match self.args.structured_mode() {
            Some(StructuredMode::Json) => &[StructuredMode::Json],
            Some(StructuredMode::Yaml) => &[StructuredMode::Yaml],
            Some(StructuredMode::Toml) => &[StructuredMode::Toml],
            None if self.files.formats.is_some() => &[
                StructuredMode::Json,
                StructuredMode::Yaml,
                StructuredMode::Toml,
            ],
            None => &[],
        }
    }

    /// Walks directories with ripgrep's filters: hidden files and ignore files are skipped
    /// unless asked otherwise, and `-g` and `-t` select files.
    fn directories<W: Write + Send>(&self, roots: &[&Path], sink: &mut Sink<'_, W>) -> Result<()> {
        let mut builder = WalkBuilder::new(roots[0]);
        for root in &roots[1..] {
            builder.add(root);
        }
        let respect = self.args.respect_ignore_files();
        builder
            .hidden(!self.args.hidden())
            .ignore(respect)
            .git_ignore(respect)
            .git_global(respect)
            .git_exclude(respect)
            .parents(respect)
            .follow_links(self.args.follow())
            .max_depth(self.args.max_depth())
            .threads(self.args.threads().map_or(0, |threads| threads.get()));
        if respect {
            builder.add_custom_ignore_filename(".rgignore");
        }
        let (selected, negated) = self.args.file_types();
        let chooses_files = !selected.is_empty() || !negated.is_empty();
        // Structured search walks only files of its formats, unless -t or -g choose files.
        let formats = if chooses_files || !self.args.globs().is_empty() {
            &[][..]
        } else {
            self.structured_formats()
        };
        if chooses_files || !formats.is_empty() {
            let mut types = TypesBuilder::new();
            types.add_defaults();
            for name in selected {
                types.select(name);
            }
            for name in negated {
                types.negate(name);
            }
            for &mode in formats {
                for glob in format_globs(mode) {
                    types.add(STRUCTURED_TYPE, glob)?;
                }
            }
            if !formats.is_empty() {
                types.select(STRUCTURED_TYPE);
            }
            builder.types(types.build().context("invalid file type")?);
        }
        if !self.args.globs().is_empty() {
            let mut overrides = OverrideBuilder::new(env::current_dir()?);
            for glob in self.args.globs() {
                overrides
                    .add(glob)
                    .with_context(|| format!("invalid glob `{glob}`"))?;
            }
            builder.overrides(overrides.build()?);
        }

        let sort = self.args.sort();
        let sequential = sort == SortBy::Path
            || self
                .args
                .threads()
                .is_some_and(|threads| threads.get() == 1);
        // Files searched one at a time each have every thread for their pieces.
        let alone = (PIECE_LEN, true);
        if sequential {
            builder.sort_by_file_path(Path::cmp);
            if sort != SortBy::Recent && sink.pretty.is_none() {
                for entry in builder.build() {
                    let mut started = false;
                    let mut emit = |bytes: &[u8]| sink.file_part(bytes, &mut started);
                    let output = self.walked_file(entry, Some(&mut emit), alone)?;
                    sink.file_part(&output.bytes, &mut started)?;
                    if self.done() {
                        break;
                    }
                }
                return Ok(());
            }
        }
        // Files print on this thread while the walk goes on, so a batch of finished files
        // can be dated together, in walk order when the walk is sequential. Ordering by last
        // change needs every file first, so all of them are dated and ordered at once.
        let (sender, receiver) = mpsc::sync_channel::<FileOutput>(PENDING_FILES);
        // Whether to stop walking after `output`: a closed channel means printing failed. Held
        // output is never written here, so searching a file can't fail to write.
        let stops = move |sender: &mpsc::SyncSender<FileOutput>, output: Result<FileOutput>| {
            output.map_or(true, |output| {
                (!output.bytes.is_empty() && sender.send(output).is_err()) || self.done()
            })
        };
        thread::scope(|scope| {
            scope.spawn(move || {
                if sequential {
                    for entry in builder.build() {
                        if stops(&sender, self.walked_file(entry, None, alone)) {
                            break;
                        }
                    }
                    return;
                }
                builder.build_parallel().run(|| {
                    let sender = sender.clone();
                    Box::new(move |entry| {
                        let output = self.walked_file(entry, None, (CONCURRENT_PIECE_LEN, false));
                        if stops(&sender, output) {
                            WalkState::Quit
                        } else {
                            WalkState::Continue
                        }
                    })
                });
            });
            let printed = match sort {
                SortBy::Recent => sink.files(receiver.into_iter().collect(), true),
                SortBy::Path | SortBy::None => sink.drain(receiver),
            };
            printed.map_err(anyhow::Error::from)
        })
    }
}

/// The length of `file` if it is a regular file, whose length is its content's.
fn regular_len(file: &File) -> Option<u64> {
    file.metadata()
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::*;
    use crate::cli::style::Depth;

    #[test]
    fn the_separator_goes_between_files_that_wrote_output() {
        let mut sink = Sink {
            out: Vec::new(),
            file_separator: b"--\n",
            wrote_file: false,
            pretty: None,
        };

        for output in [&b"a\n"[..], b"", b"b\n", b"c\n"] {
            sink.file(output).expect("writes to memory succeed");
        }

        assert_eq!(sink.out, b"a\n--\nb\n--\nc\n");
    }

    #[test]
    fn last_changes_end_the_heading_past_line_breaks_in_its_path() {
        let pretty = Pretty {
            depth: Depth::None,
            width: None,
            patterns: 1,
            values: None,
        };
        let change = LastChange::Committed(Timestamp::UNIX_EPOCH);
        let mut detail = Vec::new();
        pretty.last_change(&mut detail, change);
        let detail = String::from_utf8(detail).expect("the detail is UTF-8");
        let dated = |path: &str| {
            let mut output = FileOutput {
                bytes: format!("{path}  1 line\n1 x\n").into_bytes(),
                path: Some(PathBuf::from(path)),
                headed: true,
            };
            output.add_last_change(&pretty, change);
            String::from_utf8(output.bytes).expect("the output is UTF-8")
        };

        assert_eq!(dated("a.rs"), format!("a.rs  1 line{detail}\n1 x\n"));
        assert_eq!(dated("a\nb.rs"), format!("a\nb.rs  1 line{detail}\n1 x\n"));
    }
}
