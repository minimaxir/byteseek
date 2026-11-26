//! Searching one file's bytes and formatting what it selects.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::mem;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use anyhow::{Context, Result, bail};
use bstr::ByteSlice;
use memchr::memchr;
use smallvec::SmallVec;

use crate::cli::args::{Mode, Selection};
pub use crate::cli::pieces::Binary;
use crate::cli::pieces::{CONCURRENT_PIECE_LEN, Nuls, Piece, Pieces, read_piece_at};
use crate::cli::pretty::{Pretty, Row, Tally};
use crate::cli::printer::{CONTEXT_SEPARATOR, MATCH_SEPARATOR, Printer};
use crate::matcher::terminated_line_content_end;
use crate::{
    LineMatch, LineVisit, Match, MatchedLine, SearchError, Searcher, StructuredMode, Watch,
    Watched, count_lines, line_end_from,
};

/// A selected line: zero-based index, searched text, and matches. Inverted selections have
/// neither text nor matches.
struct Selected<'a> {
    index: usize,
    line: Option<&'a str>,
    matches: &'a [Match],
}

/// The original bytes of a file's lines, `\r` included, which are printed in place of the
/// searched text as ripgrep prints them.
enum RawLines<'a> {
    /// The searched text borrows these bytes, so a line's address gives its position in them.
    Borrowed(&'a [u8]),
    /// Every line, for inverted selections, context, and text with replaced bytes.
    Indexed(Vec<&'a [u8]>),
}

impl<'a> RawLines<'a> {
    fn new(bytes: &'a [u8], indexed: bool) -> Self {
        if !indexed {
            return Self::Borrowed(bytes);
        }
        Self::Indexed(
            bytes
                .lines_with_terminator()
                .map(|line| line.strip_suffix(b"\n").unwrap_or(line))
                .collect(),
        )
    }

    fn len(&self) -> usize {
        match self {
            Self::Borrowed(_) => 0,
            Self::Indexed(lines) => lines.len(),
        }
    }

    /// The bytes of line `index`, whose searched text is `line` if it was matched.
    fn get(&self, index: usize, line: Option<&str>) -> &'a [u8] {
        match (self, line) {
            (Self::Indexed(lines), _) => lines[index],
            (Self::Borrowed(bytes), Some(line)) => {
                let start = line.as_ptr() as usize - bytes.as_ptr() as usize;
                let len = memchr(b'\n', &bytes[start..]).unwrap_or(bytes.len() - start);
                &bytes[start..start + len]
            }
            (Self::Borrowed(_), None) => &[],
        }
    }
}

/// A row of a file's line output before it is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Lines were skipped between groups with context.
    Gap,
    /// A context line, by index.
    Context(usize),
    /// A selected line, by position in the selection.
    Selected(usize),
}

/// Selected lines laid out among their context, to write.
#[derive(Clone, Copy)]
struct Laid<'l, 'a> {
    selected: &'l [Selected<'a>],
    /// Lines selected past `--max-count`, which print as selected where they are context.
    past_max: &'l [Selected<'a>],
    slots: &'l [Slot],
}

/// Places selected lines among their context, piece by piece, merging context that overlaps
/// and marking gaps between groups. Line indexes count from the start of the file.
#[derive(Debug, Default)]
struct Layout {
    /// One past the last line laid out.
    written_end: Option<usize>,
    /// One past the last line of after-context owed.
    after_end: usize,
}

impl Layout {
    /// Lays out `selected` lines with `before` and `after` lines of context, taking context
    /// from the lines `lines` spans, then the after-context owed within them.
    fn place(
        &mut self,
        selected: &[Selected],
        (before, after): (usize, usize),
        lines: Range<usize>,
    ) -> Vec<Slot> {
        let mut slots = Vec::with_capacity(selected.len());
        for (position, line) in selected.iter().enumerate() {
            self.flush_after_context(line.index, &mut slots);
            let before_start = line
                .index
                .saturating_sub(before)
                .max(self.written_end.unwrap_or(0))
                .max(lines.start);
            if (before > 0 || after > 0) && self.written_end.is_some_and(|end| before_start > end) {
                slots.push(Slot::Gap);
            }
            slots.extend((before_start..line.index).map(Slot::Context));
            slots.push(Slot::Selected(position));
            self.written_end = Some(line.index + 1);
            self.after_end = line.index + 1 + after;
        }
        self.flush_after_context(lines.end, &mut slots);
        slots
    }

    /// Lays out the after-context owed before line `end`.
    fn flush_after_context(&mut self, end: usize, slots: &mut Vec<Slot>) {
        let from = self.written_end.unwrap_or(0);
        let to = self.after_end.min(end);
        if to > from {
            slots.extend((from..to).map(Slot::Context));
            self.written_end = Some(to);
        }
    }

    /// Whether after-context is still owed past the lines laid out.
    fn owes_context(&self) -> bool {
        self.after_end > self.written_end.unwrap_or(0)
    }
}

/// The file name patterns of each structured format, which also name it by extension.
const FORMAT_GLOBS: [(StructuredMode, &[&str]); 3] = [
    (StructuredMode::Json, &["*.json", "*.jsonl", "*.ndjson"]),
    (StructuredMode::Yaml, &["*.yaml", "*.yml"]),
    (StructuredMode::Toml, &["*.toml"]),
];

/// The file name patterns of `mode`, such as `*.yml` for YAML.
pub fn format_globs(mode: StructuredMode) -> &'static [&'static str] {
    FORMAT_GLOBS
        .iter()
        .find(|(format, _)| *format == mode)
        .map_or(&[], |(_, globs)| globs)
}

/// The structured format the extension of `path` names, case-insensitively.
fn format_of(path: &[u8]) -> Option<StructuredMode> {
    let name = path.rsplit(|&byte| matches!(byte, b'/' | b'\\')).next()?;
    let extension = &name[name.iter().rposition(|&byte| byte == b'.')? + 1..];
    FORMAT_GLOBS.iter().find_map(|(format, globs)| {
        globs
            .iter()
            .any(|glob| glob.as_bytes()[2..].eq_ignore_ascii_case(extension))
            .then_some(*format)
    })
}

/// A searcher per structured format, for structured patterns given without `--parse`: each
/// file is parsed in the format its extension names.
#[derive(Debug)]
pub struct FormatSearchers {
    json: Searcher,
    yaml: Searcher,
    toml: Searcher,
}

impl FormatSearchers {
    /// Compiles the patterns once per format.
    ///
    /// # Arguments
    ///
    /// * `build` - Compiles the patterns for one structured mode
    ///
    /// # Errors
    ///
    /// Returns the first compilation error.
    pub fn new(
        build: impl Fn(StructuredMode) -> Result<Searcher, SearchError>,
    ) -> Result<Self, SearchError> {
        Ok(Self {
            json: build(StructuredMode::Json)?,
            yaml: build(StructuredMode::Yaml)?,
            toml: build(StructuredMode::Toml)?,
        })
    }

    /// The searcher for the format the extension of `path` names.
    fn for_path(&self, path: &[u8]) -> Option<&Searcher> {
        Some(match format_of(path)? {
            StructuredMode::Json => &self.json,
            StructuredMode::Yaml => &self.yaml,
            StructuredMode::Toml => &self.toml,
        })
    }
}

/// Writing output failed, which ends the whole search instead of one input's.
#[derive(Debug, thiserror::Error)]
#[error("failed to write output")]
pub struct WriteError(#[source] pub io::Error);

/// Receives an input's output as its search finds it, on whichever thread finds it.
pub type Emit<'e> = &'e mut (dyn FnMut(&[u8]) -> io::Result<()> + Send);

/// An input searched in pieces.
#[derive(Debug)]
pub struct Source<'a, R> {
    /// The name printed for the input.
    pub name: &'a [u8],
    /// Reads the input.
    pub reader: R,
    /// The input's length, when known.
    pub len: Option<u64>,
    /// A regular file holding the input, which can be read again to look ahead.
    pub path: Option<&'a Path>,
    /// What a NUL byte does to the search.
    pub binary: Binary,
    /// Most bytes a piece reads, unless one line is longer. Pieces searched on many threads
    /// read at most [`CONCURRENT_PIECE_LEN`].
    pub piece_len: usize,
    /// A regular file's pieces may be read and searched on every thread of the pool, as
    /// when the file is searched alone.
    pub parallel: bool,
}

impl<R> Source<'_, R> {
    /// Whether the input is a regular file whose pieces are read and searched on many
    /// threads: one long enough to give each thread a piece.
    pub fn reads_in_parallel(&self) -> bool {
        self.parallel_file().is_some()
    }

    fn parallel_file(&self) -> Option<ParallelFile<'_>> {
        let (path, len) = (self.path?, self.len?);
        let piece_len = self.piece_len.min(CONCURRENT_PIECE_LEN);
        // Asking for the pool's threads starts the pool, which a short file never needs.
        if !self.parallel || len <= 2 * piece_len as u64 {
            return None;
        }
        let threads = rayon::current_num_threads();
        (threads > 1 && len > threads.saturating_mul(piece_len) as u64).then_some(ParallelFile {
            name: self.name,
            path,
            len,
            binary: self.binary,
            piece_len,
            threads,
        })
    }
}

/// A regular file searched a piece at a time on many threads.
#[derive(Debug, Clone, Copy)]
struct ParallelFile<'a> {
    name: &'a [u8],
    path: &'a Path,
    len: u64,
    binary: Binary,
    piece_len: usize,
    threads: usize,
}

/// Passes the output from `start` on to `emit`, if any, and drops it from `out`.
fn emit_from(
    out: &mut Vec<u8>,
    start: usize,
    emit: &mut Option<Emit<'_>>,
) -> Result<(), WriteError> {
    if let Some(emit) = emit {
        emit(&out[start..]).map_err(WriteError)?;
        out.truncate(start);
    }
    Ok(())
}

/// What a file's search carries from one piece of it to the next.
#[derive(Debug, Default)]
struct Progress {
    /// Index of the first line that the next piece reads anew.
    next_line: usize,
    /// Lines selected so far.
    selected: usize,
    /// Matches counted so far, for `--count-matches`.
    matches: usize,
    /// The file's heading was written.
    headed: bool,
    layout: Layout,
}

/// What searching one piece of a file found, before it adds to the file's output.
enum Scan<'a> {
    /// Lines with a match, counted as bytes.
    MatchingLines(usize),
    /// Matches in the piece's text.
    Matches(usize),
    /// Whether the piece's text has a match.
    Presence(bool),
    /// Lines selected past the `kept` lines an earlier piece searched, by index from the
    /// piece's first line.
    Lines {
        selected: Vec<Selected<'a>>,
        raw: RawLines<'a>,
        kept: usize,
        /// Lines past the kept ones, when they are counted.
        new_lines: usize,
    },
    /// A piece's whole lines, whose output is formatted straight from the searcher's
    /// matches in them: see [`FileSearcher::formats_bytes`]. With their line feeds, when
    /// those were counted as the piece was read.
    Bytes(&'a [u8], Option<usize>),
    /// Output formatted apart from the rest of the file, which adding swaps in for the
    /// output's buffer, whether it `selected` a line, and the lines its piece read anew when
    /// lines are numbered.
    Formatted {
        output: &'a mut Vec<u8>,
        selected: bool,
        new_lines: usize,
    },
}

impl Scan<'_> {
    /// The scan, unless it borrows from its piece.
    fn into_owned(self) -> Result<Scan<'static>, Self> {
        match self {
            Self::MatchingLines(count) => Ok(Scan::MatchingLines(count)),
            Self::Matches(count) => Ok(Scan::Matches(count)),
            Self::Presence(found) => Ok(Scan::Presence(found)),
            Self::Lines { .. } | Self::Bytes(..) | Self::Formatted { .. } => Err(self),
        }
    }
}

/// What a piece of a file holds past its first NUL byte.
#[derive(Debug, Clone, Copy, Default)]
struct Place {
    /// The file's first NUL byte, when the piece ends at its line.
    nul: Option<usize>,
    /// What the piece selects past the NUL byte's line.
    past_nul: PastNul,
}

/// Lines selected past a NUL byte's line, with NUL bytes as line feeds, which decide whether
/// a file with [`Binary::Notice`] matched.
#[derive(Debug, Clone, Copy, Default)]
struct PastNul {
    /// A line is selected.
    selected: bool,
    /// The first line is selected: with context before selected lines, any other selected
    /// line follows a line ripgrep prints as context, where its search stops.
    first_selected: bool,
}

/// A file's output as its pieces add to it in order, up to the line of a NUL byte that stops
/// it.
struct Assembly<'o, 'e> {
    progress: Progress,
    binary: Binary,
    /// Later pieces can still change what the file's lines add to the output.
    searching: bool,
    /// The first NUL byte, once the search reached its line.
    nul: Option<usize>,
    /// Lines after selected ones were still owed as context at the NUL byte's line.
    context_owed_at_nul: bool,
    past_nul: PastNul,
    /// Output, of which bytes from `start` on are the file's and not yet emitted.
    out: &'o mut Vec<u8>,
    start: usize,
    /// Receives output as it is added; without it, all output is held.
    emit: Option<Emit<'e>>,
    /// No later piece can change the output.
    settled: bool,
}

impl<'o, 'e> Assembly<'o, 'e> {
    fn new(binary: Binary, out: &'o mut Vec<u8>, emit: Option<Emit<'e>>) -> Self {
        Self {
            progress: Progress::default(),
            binary,
            searching: true,
            nul: None,
            context_owed_at_nul: false,
            past_nul: PastNul::default(),
            start: out.len(),
            out,
            emit,
            settled: false,
        }
    }

    /// Adds the next piece of the file, at `place`, with what scanning it found if the file
    /// was still searched, and emits its output.
    ///
    /// # Errors
    ///
    /// Returns a [`WriteError`] if emitting fails.
    fn add(
        &mut self,
        files: &FileSearcher,
        name: &[u8],
        place: Place,
        scan: Option<Scan>,
    ) -> Result<(), WriteError> {
        if self.searching
            && let Some(scan) = scan
        {
            if self.nul.is_none() {
                self.searching = files.apply(name, scan, &mut self.progress, self.out);
                emit_from(self.out, self.start, &mut self.emit)?;
            } else {
                // Past the NUL byte's line, only whether a line is selected matters.
                let selected = self.progress.selected;
                files.apply(name, scan, &mut self.progress, &mut Vec::new());
                self.past_nul.selected |= self.progress.selected > selected;
                self.progress.selected = selected;
            }
        }
        // A search that has what it needs before the NUL byte's line never reaches it, but
        // ripgrep's `--files-without-match` reads on to one.
        let reads_on = self.binary == Binary::Quit && files.mode == Mode::FilesWithoutMatch;
        if self.nul.is_none() && place.nul.is_some() && (self.searching || reads_on) {
            self.nul = place.nul;
            self.context_owed_at_nul = self.progress.layout.owes_context();
            self.past_nul = place.past_nul;
        } else if self.nul.is_some() {
            self.past_nul.selected |= place.past_nul.selected;
        }
        self.settled = match self.nul {
            None => !self.searching && !reads_on,
            Some(_) => {
                self.binary == Binary::Quit
                    || self.context_owed_at_nul
                    || self.past_nul.selected
                    || (self.progress.selected > 0 && files.context == (0, 0))
            }
        };
        Ok(())
    }

    /// Writes what the file's pieces add up to, and emits the output not yet emitted.
    ///
    /// # Returns
    ///
    /// Whether the file counts as a success for the exit status.
    ///
    /// # Errors
    ///
    /// Returns a [`WriteError`] if emitting fails.
    fn finish(mut self, files: &FileSearcher, name: &[u8]) -> Result<bool, WriteError> {
        let selected = self.progress.selected > 0;
        let matched = match (self.nul, self.binary) {
            (None, _) | (_, Binary::Search) => files.finish(name, &self.progress, self.out),
            (Some(nul_offset), Binary::Quit) if files.prints_lines() => {
                if selected {
                    files.printer.binary_stopped(self.out, name, nul_offset);
                }
                selected
            }
            // A stopped search prints no count or path, which only `--files-without-match`
            // counts as a success.
            (Some(_), Binary::Quit) => files.mode == Mode::FilesWithoutMatch,
            (Some(nul_offset), Binary::Notice) => {
                // ripgrep's search past the NUL byte's line stops at the first line it would
                // print, after the separator before it.
                let past = self.past_nul.selected && !self.context_owed_at_nul;
                let counted = past && (files.context.0 == 0 || self.past_nul.first_selected);
                if selected && past && files.context != (0, 0) {
                    files.printer.context_break(self.out);
                }
                if selected || counted {
                    files.printer.binary_match(self.out, name, nul_offset);
                }
                selected || counted
            }
        };
        emit_from(self.out, self.start, &mut self.emit)?;
        Ok(matched)
    }
}

/// Most output that pieces formatted apart may leave in turns not yet run before their
/// threads wait for their own turns instead.
const LEFT_OUTPUT_LIMIT: usize = 2 * 1024 * 1024;

/// What the threads searching a file's pieces share.
struct Shared<'f, 'o, 'e> {
    file: ParallelFile<'f>,
    /// Lines kept before each piece for context.
    keep: usize,
    /// Numbers the lines of pieces formatted apart.
    numbering: Turns<'f, Numbering>,
    output: Turns<'f, Output<'o, 'e>>,
    /// Whether the file is still searched, as of the last turn run.
    searching: &'f AtomicBool,
    /// Bytes of formatted output in turns left and not yet run.
    left_output: &'f AtomicUsize,
    /// Buffers that formatted output was added in, to format again.
    spares: &'f Mutex<Vec<Vec<u8>>>,
    /// No later piece can change the output, so the search ends.
    ended: AtomicBool,
}

impl<'f, 'o, 'e> Shared<'f, 'o, 'e> {
    /// Ends the search, waking every thread waiting for a turn.
    fn end(&self) {
        self.ended.store(true, Ordering::Relaxed);
        self.numbering.wake_all();
        self.output.wake_all();
    }

    /// Runs `turn` in the turn of piece `index` in `turns`, waiting for it.
    fn take<T>(&self, turns: &Turns<'f, T>, index: usize, turn: impl FnOnce(&mut T) -> bool) {
        if turns.take(index, &self.ended, turn) {
            self.end();
        }
    }

    /// Leaves `turn` to run in the turn of piece `index` in `turns`, on whichever thread
    /// reaches it.
    fn leave<T>(&self, turns: &Turns<'f, T>, index: usize, turn: Turn<'f, T>) {
        if turns.leave(index, &self.ended, turn) {
            self.end();
        }
    }

    /// Ends the search in the turn of piece `index`, which failed to read.
    fn fail(&self, index: usize, error: io::Error) {
        self.take(&self.output, index, |output| {
            output.error = Some(anyhow::Error::from(error).context("read failed"));
            true
        });
    }

    /// A buffer to format a piece's output in.
    fn spare(&self) -> Vec<u8> {
        let mut spares = self.spares.lock().unwrap_or_else(PoisonError::into_inner);
        spares.pop().unwrap_or_default()
    }

    /// Returns a buffer that formatted output was added in.
    fn put_back(spares: &Mutex<Vec<Vec<u8>>>, mut buffer: Vec<u8>) {
        buffer.clear();
        spares
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(buffer);
    }
}

/// A piece's turn, which changes the state and returns whether the search ends.
type Turn<'f, T> = Box<dyn FnOnce(&mut T) -> bool + Send + 'f>;

/// State that a file's pieces change one at a time, in order. A piece's thread either waits
/// for its turn or leaves the turn to whichever thread reaches it and goes on. Turns run
/// outside the queue's lock, so leaving a turn never waits on one running.
struct Turns<'f, T> {
    queue: Mutex<Queue<'f, T>>,
    /// Only the thread running turns locks it, but for reads of passed turns.
    value: Mutex<T>,
    /// Wakes each thread, by its first piece, when the turn of one of its pieces comes or
    /// passes.
    wake: Vec<Condvar>,
}

struct Queue<'f, T> {
    /// The piece whose turn it is.
    next: usize,
    /// A thread is running turns, from `next` on.
    running: bool,
    /// Turns left for later pieces, by piece.
    left: BTreeMap<usize, Turn<'f, T>>,
}

impl<'f, T> Turns<'f, T> {
    fn new(value: T, threads: usize) -> Self {
        Self {
            queue: Mutex::new(Queue {
                next: 0,
                running: false,
                left: BTreeMap::new(),
            }),
            value: Mutex::new(value),
            wake: (0..threads).map(|_| Condvar::new()).collect(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queue<'f, T>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits on piece `index`'s thread's wake.
    fn wait<'s>(
        &self,
        index: usize,
        queue: MutexGuard<'s, Queue<'f, T>>,
    ) -> MutexGuard<'s, Queue<'f, T>> {
        (self.wake[index % self.wake.len()].wait(queue)).unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `turn` on this thread in the turn of piece `index`, waiting for it, unless
    /// `ended` says the search ended first.
    ///
    /// # Returns
    ///
    /// Whether a turn ended the search.
    fn take(&self, index: usize, ended: &AtomicBool, turn: impl FnOnce(&mut T) -> bool) -> bool {
        let mut queue = self.lock();
        while (queue.next != index || queue.running) && !ended.load(Ordering::Relaxed) {
            queue = self.wait(index, queue);
        }
        if ended.load(Ordering::Relaxed) {
            return false;
        }
        queue.running = true;
        drop(queue);
        self.run(turn)
    }

    /// Runs `turn` in the turn of piece `index`: now if it has come, else on the thread that
    /// reaches it.
    ///
    /// # Returns
    ///
    /// Whether a turn ended the search.
    fn leave(&self, index: usize, ended: &AtomicBool, turn: Turn<'f, T>) -> bool {
        let mut queue = self.lock();
        if ended.load(Ordering::Relaxed) {
            return false;
        }
        if queue.next != index || queue.running {
            queue.left.insert(index, turn);
            return false;
        }
        queue.running = true;
        drop(queue);
        self.run(turn)
    }

    /// Reads the state once the turn of piece `index` has passed, unless the search ends
    /// first.
    fn read_past<R>(
        &self,
        index: usize,
        ended: &AtomicBool,
        read: impl FnOnce(&T) -> R,
    ) -> Option<R> {
        let mut queue = self.lock();
        while queue.next <= index && !ended.load(Ordering::Relaxed) {
            queue = self.wait(index, queue);
        }
        drop(queue);
        let value = self.value.lock().unwrap_or_else(PoisonError::into_inner);
        (!ended.load(Ordering::Relaxed)).then(|| read(&value))
    }

    /// Runs `turn`, whose piece's turn this thread claimed, then the turns left for the
    /// pieces after it, passing the turn on and waking the threads whose pieces' turns passed
    /// or came.
    ///
    /// # Returns
    ///
    /// Whether a turn ended the search.
    fn run(&self, turn: impl FnOnce(&mut T) -> bool) -> bool {
        let threads = self.wake.len();
        let mut value = self.value.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ends = turn(&mut value);
        loop {
            let mut queue = self.lock();
            self.wake[queue.next % threads].notify_one();
            queue.next += 1;
            let next = queue.next;
            let turn = if ends { None } else { queue.left.remove(&next) };
            let Some(turn) = turn else {
                queue.running = false;
                self.wake[next % threads].notify_one();
                return ends;
            };
            drop(queue);
            ends = turn(&mut value);
        }
    }

    /// Wakes every thread waiting for a turn. Taking the lock first lets a thread that just
    /// saw the search going on start waiting.
    fn wake_all(&self) {
        let _queue = self.lock();
        self.wake.iter().for_each(Condvar::notify_one);
    }

    fn into_inner(self) -> T {
        (self.value.into_inner()).unwrap_or_else(PoisonError::into_inner)
    }
}

/// Numbers the lines of pieces formatted apart.
struct Numbering {
    /// The number of the next piece's first line, counting from zero.
    next_line: usize,
    /// The first line of the piece each thread, by its first piece, numbered last.
    first_lines: Vec<usize>,
}

/// The file's output, which its pieces add to in their turns.
struct Output<'o, 'e> {
    assembly: Assembly<'o, 'e>,
    /// Reading the file or writing its output failed, which ends its search.
    error: Option<anyhow::Error>,
}

impl Output<'_, '_> {
    /// Adds the next piece of `file`, at `place`, with what scanning it found, and records
    /// whether the file is still `searching`.
    ///
    /// # Returns
    ///
    /// Whether the search ends.
    fn add(
        &mut self,
        files: &FileSearcher,
        file: &ParallelFile,
        searching: &AtomicBool,
        (place, scan): (Place, Option<Scan>),
    ) -> bool {
        let added = (self.assembly).add(files, file.name, place, scan);
        searching.store(self.assembly.searching, Ordering::Relaxed);
        match added {
            Ok(()) => self.assembly.settled,
            Err(error) => {
                self.error = Some(error.into());
                true
            }
        }
    }
}

/// Writes the selected lines of a piece's bytes as a [`FileSearcher`] prints them, numbering
/// lines only when their numbers print, and copying runs of lines printed bare whole.
struct LineWriter<'w> {
    files: &'w FileSearcher<'w>,
    name: &'w [u8],
    bytes: &'w [u8],
    /// The number of the piece's first line, counting from zero.
    first_line: usize,
    /// Bytes of lines that print bare, as they are, not yet copied.
    run: Range<usize>,
    out: &'w mut Vec<u8>,
}

impl LineWriter<'_> {
    /// Writes `line` with `matches`, the ones of its matches that select it.
    fn matched(&mut self, line: &MatchedLine, matches: &[Match]) {
        let printer = self.files.printer;
        let number = self.first_line + line.index + 1;
        if self.files.mode == Mode::OnlyMatching {
            for found in matches {
                let text = &self.bytes[line.start + found.start..line.start + found.end];
                (printer).only_match(self.out, self.name, number, found.start + 1, text);
            }
        } else if printer.prints_lines_bare() {
            self.copy(line.start..(line.end + 1).min(self.bytes.len()));
        } else {
            let bytes = &self.bytes[line.start..line.end];
            printer.line(self.out, self.name, number, bytes, matches, MATCH_SEPARATOR);
        }
    }

    /// Writes the whole lines spanning `lines`, the first of them line `index` of the piece,
    /// which no match selected, as inverted lines; with `--only-matching` too, as in ripgrep.
    ///
    /// # Returns
    ///
    /// Whether there were any.
    fn unmatched(&mut self, lines: Range<usize>, index: usize) -> bool {
        if lines.is_empty() {
            return false;
        }
        let printer = self.files.printer;
        if printer.prints_lines_bare() {
            self.copy(lines);
            return true;
        }
        let mut start = lines.start;
        for number in self.first_line + index + 1.. {
            let end = line_end_from(&self.bytes[..lines.end], start);
            let bytes = &self.bytes[start..end];
            printer.line(self.out, self.name, number, bytes, &[], MATCH_SEPARATOR);
            start = end + 1;
            if start >= lines.end {
                break;
            }
        }
        true
    }

    /// Adds whole lines spanning `lines` to the run copied as they are.
    fn copy(&mut self, lines: Range<usize>) {
        if self.run.end != lines.start {
            self.flush();
            self.run.start = lines.start;
        }
        self.run.end = lines.end;
    }

    /// Copies the run of lines printed bare, ending the input's last line with a line feed.
    fn flush(&mut self) {
        let run = mem::replace(&mut self.run, 0..0);
        if run.is_empty() {
            return;
        }
        let bytes = &self.bytes[run];
        self.out.extend_from_slice(bytes);
        if bytes.last() != Some(&b'\n') {
            self.out.push(b'\n');
        }
    }
}

/// Searches files and formats their output.
#[derive(Debug, Clone, Copy)]
pub struct FileSearcher<'a> {
    /// The compiled patterns.
    pub searcher: &'a Searcher,
    /// Which lines are selected.
    pub selection: Selection,
    /// What each file contributes to the output.
    pub mode: Mode,
    /// Print counts of zero.
    pub include_zero: bool,
    /// Line context before and after selected lines.
    pub context: (usize, usize),
    /// Output formatting.
    pub printer: Printer,
    /// The `--pretty` layout, which replaces the printer's.
    pub pretty: Option<Pretty<'a>>,
    /// Totals for the `--pretty` summary.
    pub tally: Option<&'a Tally>,
    /// Files are parsed as structured documents.
    pub structured: bool,
    /// Searchers chosen by extension, when files are parsed without `--parse`.
    pub formats: Option<&'a FormatSearchers>,
    /// No match spans lines, so a file's matches and presence add up over its lines. Fuzzy
    /// matches and patterns with line feeds can span lines.
    pub line_local: bool,
}

impl FileSearcher<'_> {
    /// Whether the output is a count of matching lines that a stream can give without reading
    /// the whole input into memory.
    pub fn counts_by_stream(&self) -> bool {
        self.mode == Mode::Count
            && !self.selection.invert
            && !self.selection.whole_line
            && !self.structured
            && self.tally.is_none()
    }

    /// Whether a file searched in pieces of whole lines gives the output it gives searched
    /// whole. Structured documents parse whole, `--pretty` lays a file's lines out together,
    /// and the presence and match counts of searchers whose matches can span lines search the
    /// whole text.
    pub fn streams(&self) -> bool {
        !self.structured
            && self.pretty.is_none()
            && self.tally.is_none()
            && (self.line_local || !self.searches_whole_text())
    }

    /// Whether every match counts, without selecting lines: `--count-matches`, or presence.
    fn searches_whole_text(&self) -> bool {
        self.plain()
            && match self.mode {
                Mode::Quiet | Mode::FilesWithMatches | Mode::FilesWithoutMatch => true,
                Mode::CountMatches => self.selection.max_count.is_none(),
                Mode::Lines | Mode::OnlyMatching | Mode::Count => false,
            }
    }

    /// Whether lines are selected by their matches alone; the summary counts every match, so
    /// its searches take the full path.
    fn plain(&self) -> bool {
        !self.selection.invert && !self.selection.whole_line && self.tally.is_none()
    }

    /// Counts the matching lines of `reader` and writes the count; needs
    /// [`Self::counts_by_stream`].
    ///
    /// # Returns
    ///
    /// Whether a line was selected.
    ///
    /// # Errors
    ///
    /// Returns the reader's error if a read fails.
    pub fn count_stream(&self, path: &[u8], reader: impl Read, out: &mut Vec<u8>) -> Result<bool> {
        let count = self
            .searcher
            .count_matching_lines_in_reader(reader)
            .context("read failed")?;
        Ok(self.write_count(path, self.capped(count), out))
    }

    /// Searches `bytes`, appending what they contribute to the output to `out`, doing with
    /// NUL bytes what `binary` says. Bytes that are not UTF-8 are searched with replacement
    /// characters in their place.
    ///
    /// # Returns
    ///
    /// Whether the file counts as a success for the exit status: a line was selected, or for
    /// `--files-without-match`, none was. Then whether lines were written, under a heading.
    ///
    /// # Errors
    ///
    /// Returns an error if a structured document does not parse or a strict predicate fails.
    pub fn search(
        &self,
        path: &[u8],
        bytes: &mut [u8],
        binary: Binary,
        out: &mut Vec<u8>,
    ) -> Result<(bool, bool)> {
        let mut nuls = Nuls::new(binary);
        nuls.scan(bytes, 0);
        let bytes = &*bytes;
        if self.counts_by_stream() && !binary.stops() {
            return Ok((self.count_stream(path, bytes, out)?, false));
        }
        let whole = Piece::new(bytes, (0, 0), 0, nuls.first());
        // A file whose search stops before its first line is not searched.
        if let Some(tally) = self.tally
            && (whole.nul.is_none() || !whole.bytes.is_empty())
        {
            tally.searched(whole.bytes.len());
        }
        if self.structured {
            return self.search_document(path, bytes, whole.nul, out);
        }
        let place = self.place(path, &whole, binary);
        let mut assembly = Assembly::new(binary, out, None);
        self.scan_piece(&whole, true, |scan| assembly.add(self, path, place, scan))?;
        // Only lines were written before the notice or warning that finishing may write.
        let headed = assembly.out.len() > assembly.start;
        Ok((assembly.finish(self, path)?, headed))
    }

    /// Searches a structured document, which a NUL byte at `nul` stops before it parses or,
    /// with a notice, replaces the output of. Returns as [`Self::search`] does.
    fn search_document(
        &self,
        path: &[u8],
        text: &[u8],
        nul: Option<usize>,
        out: &mut Vec<u8>,
    ) -> Result<(bool, bool)> {
        let searcher = match self.formats {
            None => self.searcher,
            Some(formats) => match formats.for_path(path) {
                Some(searcher) => searcher,
                None => bail!(
                    "can't tell the format from the file name; pass --parse json, yaml, or \
                     toml for structured patterns"
                ),
            },
        };
        let Some(nul_offset) = nul else {
            let len = out.len();
            let matched = self.search_structured(searcher, path, &lossy_text(text), out)?;
            return Ok((matched, out.len() > len));
        };
        if !self.prints_lines() {
            return Ok((false, false));
        }
        let matched = self.search_structured(searcher, path, &lossy_text(text), &mut Vec::new())?;
        if matched {
            self.printer.binary_match(out, path, nul_offset);
        }
        Ok((matched, false))
    }

    /// Searches an input a piece of whole lines at a time; needs [`Self::streams`]. Its output
    /// equals [`Self::search`]'s on the whole input, with the source's `binary` deciding what
    /// a NUL byte does. With `emit`, output goes to it as it is found; without, it is appended
    /// to `out`.
    ///
    /// # Arguments
    ///
    /// * `source` - The input and what is known about it
    /// * `out` - Holds output not yet emitted
    /// * `emit` - Receives output as it is found
    ///
    /// # Returns
    ///
    /// Whether the input counts as a success for the exit status.
    ///
    /// # Errors
    ///
    /// Returns the error of a failed read, or a [`WriteError`] if `emit` fails.
    pub fn search_reader<R: Read>(
        &self,
        source: Source<'_, R>,
        out: &mut Vec<u8>,
        emit: Option<Emit<'_>>,
    ) -> Result<bool> {
        if let Some(file) = source.parallel_file() {
            return self.search_in_parallel(file, out, emit);
        }
        let Source {
            name,
            reader,
            len,
            binary,
            piece_len,
            ..
        } = source;
        // A search that formats bytes finds their NUL bytes in its own pass over them.
        let defers_nuls = binary != Binary::Search && self.formats_bytes();
        let mut pieces = Pieces::new(reader, len, piece_len, binary).deferring_nuls(defers_nuls);
        let mut assembly = Assembly::new(binary, out, emit);
        let keep = self.kept_lines();
        let mut formatted = Vec::new();
        while !assembly.settled
            && let Some(mut piece) = pieces.next(keep).context("read failed")?
        {
            if piece.unchecked {
                let how = LineVisit {
                    nuls: true,
                    ..LineVisit::default()
                };
                let numbered = (assembly.progress.next_line, how);
                let scan = (assembly.searching && self.prints_bytes_as_text(piece.bytes))
                    .then(|| self.format_bytes(name, piece.bytes, numbered, &mut formatted))
                    .flatten();
                if let Some((selected, new_lines)) = scan {
                    let scan = Scan::Formatted {
                        output: &mut formatted,
                        selected,
                        new_lines,
                    };
                    assembly.add(self, name, Place::default(), Some(scan))?;
                    continue;
                }
                // What was formatted before a NUL byte turned up is not the piece's output.
                formatted.clear();
                piece = pieces.checked();
            }
            let place = self.place(name, &piece, binary);
            self.scan_piece(&piece, assembly.searching, |scan| {
                assembly.add(self, name, place, scan)
            })?;
        }
        Ok(assembly.finish(self, name)?)
    }

    /// Searches a regular file on many threads, each reading and searching every so many of
    /// its pieces, which add to the output in order and emit it in their turns.
    fn search_in_parallel(
        &self,
        file: ParallelFile<'_>,
        out: &mut Vec<u8>,
        emit: Option<Emit<'_>>,
    ) -> Result<bool> {
        let pieces = file.len.div_ceil(file.piece_len as u64);
        let threads = file
            .threads
            .min(usize::try_from(pieces).unwrap_or(usize::MAX));
        let (searching, left_output) = (AtomicBool::new(true), AtomicUsize::new(0));
        let spares = Mutex::new(Vec::new());
        let numbering = Numbering {
            next_line: 0,
            first_lines: vec![0; threads],
        };
        let output = Output {
            assembly: Assembly::new(file.binary, out, emit),
            error: None,
        };
        let shared = Shared {
            file,
            keep: self.kept_lines(),
            numbering: Turns::new(numbering, threads),
            output: Turns::new(output, threads),
            searching: &searching,
            left_output: &left_output,
            spares: &spares,
            ended: AtomicBool::new(false),
        };
        thread::scope(|scope| {
            for first in 0..threads {
                let shared = &shared;
                scope.spawn(move || self.search_share(shared, first));
            }
        });
        let output = shared.output.into_inner();
        if let Some(error) = output.error {
            return Err(error);
        }
        Ok(output.assembly.finish(self, file.name)?)
    }

    /// Reads and searches the pieces of a shared file from piece `first` on, as many pieces
    /// apart as there are threads, adding each to the output in its turn.
    fn search_share<'f>(&'f self, shared: &Shared<'f, '_, '_>, first: usize) {
        let file = shared.file;
        let mut reader = match File::open(file.path) {
            Ok(reader) => reader,
            Err(error) => return shared.fail(first, error),
        };
        let numbers = self.formats_alone() && self.printer.line_number;
        // Pieces whose bytes format as they are get one pass that both counts their lines
        // and looks for NUL bytes, in place of one of each.
        let watch = Watch {
            nuls: file.binary != Binary::Search && self.formats_bytes(),
            feeds: numbers,
        };
        let read_binary = if watch.nuls {
            Binary::Search
        } else {
            file.binary
        };
        let threads = shared.output.wake.len();
        let mut buffer = Vec::new();
        let pieces =
            usize::try_from(file.len.div_ceil(file.piece_len as u64)).unwrap_or(usize::MAX);
        for index in (first..pieces).step_by(threads) {
            if shared.ended.load(Ordering::Relaxed) {
                return;
            }
            let start = index as u64 * file.piece_len as u64;
            let segment = start..file.len.min(start + file.piece_len as u64);
            let at = segment.clone();
            let read = read_piece_at(&mut reader, at, shared.keep, read_binary, &mut buffer);
            let mut piece = match read {
                Ok(piece) => piece,
                Err(error) => return shared.fail(index, error),
            };
            let mut watched = Watched::default();
            if let Some(piece) = &mut piece {
                watched.pass(piece.bytes, watch);
                piece.feeds = (watch.feeds && !watched.nul).then_some(watched.feeds);
            }
            if watched.nul {
                // Read again, with the NUL bytes as the search needs them.
                let read =
                    read_piece_at(&mut reader, segment, shared.keep, file.binary, &mut buffer);
                piece = match read {
                    Ok(piece) => piece,
                    Err(error) => return shared.fail(index, error),
                };
            }
            if numbers {
                let new_lines = (piece.as_ref()).map_or(0, |piece| match piece.feeds {
                    // Counted over the whole piece, which numbered lines keep no lines before.
                    Some(feeds) if piece.new_start == 0 => {
                        feeds + usize::from(piece.bytes.last().is_some_and(|&byte| byte != b'\n'))
                    }
                    Some(_) | None => count_lines(piece.new_bytes()),
                });
                shared.leave(
                    &shared.numbering,
                    index,
                    Box::new(move |numbering: &mut Numbering| {
                        numbering.first_lines[index % threads] = numbering.next_line;
                        numbering.next_line += new_lines;
                        false
                    }),
                );
            }
            let Some(piece) = piece else {
                shared.leave(&shared.output, index, Box::new(|_| false));
                continue;
            };
            let place = self.place(file.name, &piece, file.binary);
            let searching = shared.searching.load(Ordering::Relaxed);
            self.scan_piece(&piece, searching, |scan| {
                self.pass_on(shared, (index, place), scan);
            });
        }
    }

    /// Adds what scanning piece `index` at `place` found to the output in its turn, formatting
    /// its lines first when pieces format apart. The turn is left to whichever thread reaches
    /// it unless the scan borrows from the piece or much formatted output waits already.
    fn pass_on<'f>(
        &'f self,
        shared: &Shared<'f, '_, '_>,
        (index, place): (usize, Place),
        scan: Option<Scan>,
    ) {
        let (file, searching, spares) = (shared.file, shared.searching, shared.spares);
        let lines = match scan.map(Scan::into_owned) {
            None => {
                let turn =
                    move |output: &mut Output| output.add(self, &file, searching, (place, None));
                return shared.leave(&shared.output, index, Box::new(turn));
            }
            Some(Ok(owned)) => {
                let turn = move |output: &mut Output| {
                    output.add(self, &file, searching, (place, Some(owned)))
                };
                return shared.leave(&shared.output, index, Box::new(turn));
            }
            Some(Err(lines)) if self.formats_alone() => lines,
            Some(Err(borrowed)) => {
                let turn = |output: &mut Output| {
                    output.add(self, &file, searching, (place, Some(borrowed)))
                };
                return shared.take(&shared.output, index, turn);
            }
        };
        let Some(first_line) = self.first_line(shared, index) else {
            return;
        };
        let mut formatted = shared.spare();
        let (selected, new_lines) = self.format_alone(file.name, lines, first_line, &mut formatted);
        let len = formatted.len();
        let left_output = shared.left_output;
        let waiting = left_output.fetch_add(len, Ordering::Relaxed) + len;
        let turn: Turn<Output> = Box::new(move |output: &mut Output| {
            let scan = Scan::Formatted {
                output: &mut formatted,
                selected,
                new_lines,
            };
            let ends = output.add(self, &file, searching, (place, Some(scan)));
            left_output.fetch_sub(len, Ordering::Relaxed);
            Shared::put_back(spares, formatted);
            ends
        });
        // Past the limit, this thread holds its output until its turn instead of going on.
        if waiting > LEFT_OUTPUT_LIMIT {
            shared.take(&shared.output, index, turn);
        } else {
            shared.leave(&shared.output, index, turn);
        }
    }

    /// The number of piece `index`'s first line, counting from zero, once the pieces before
    /// it are numbered; zero when line numbers don't print, and `None` if the search ends.
    fn first_line(&self, shared: &Shared, index: usize) -> Option<usize> {
        if !self.printer.line_number {
            return Some(0);
        }
        let threads = shared.numbering.wake.len();
        (shared.numbering).read_past(index, &shared.ended, |numbering| {
            numbering.first_lines[index % threads]
        })
    }

    /// What `piece` holds past its file's first NUL byte, as `binary` treats it.
    fn place(&self, name: &[u8], piece: &Piece, binary: Binary) -> Place {
        let past_nul = if binary == Binary::Notice && piece.nul.is_some() {
            let past = piece.past_nul;
            let first_end = memchr(b'\n', past).map_or(past.len(), |at| at + 1);
            PastNul {
                selected: self.selects_a_line(name, past),
                first_selected: self.context.0 > 0 && self.selects_a_line(name, &past[..first_end]),
            }
        } else {
            PastNul::default()
        };
        Place {
            nul: piece.nul,
            past_nul,
        }
    }

    /// Whether whole lines `bytes` select a line, apart from the rest of their file.
    fn selects_a_line(&self, name: &[u8], bytes: &[u8]) -> bool {
        let mut progress = Progress::default();
        let piece = Piece::new(bytes, (0, 0), 0, None);
        self.scan_piece(&piece, true, |scan| {
            if let Some(scan) = scan {
                self.apply(name, scan, &mut progress, &mut Vec::new());
            }
        });
        progress.selected > 0
    }

    /// Whether files print their lines or matches, rather than a count or a path.
    pub fn prints_lines(&self) -> bool {
        matches!(self.mode, Mode::Lines | Mode::OnlyMatching)
    }

    /// Whether a piece's output depends on nothing but its lines and their numbers: without
    /// context that merges with other pieces' or a cap on the lines selected, pieces format
    /// their lines apart.
    fn formats_alone(&self) -> bool {
        matches!(self.mode, Mode::Lines | Mode::OnlyMatching)
            && self.context == (0, 0)
            && self.selection.max_count.is_none()
            && self.pretty.is_none()
            && self.tally.is_none()
    }

    /// Whether pieces format their lines apart straight from the searcher's matches in their
    /// bytes, with no text decoded: see [`Searcher::matches_bytes_as_text`].
    fn formats_bytes(&self) -> bool {
        self.formats_alone() && self.searcher.matches_bytes_as_text()
    }

    /// Whether a piece of `bytes` prints as it would from its text. Matches in bytes are the
    /// text's, but where they sit in a line with replaced bytes, which highlights and columns
    /// show, can differ.
    fn prints_bytes_as_text(&self, bytes: &[u8]) -> bool {
        !(self.printer.color || self.printer.column) || simdutf8::basic::from_utf8(bytes).is_ok()
    }

    /// Formats the lines that `bytes`, whole lines of a file from its line `first_line` on,
    /// select into `out`, straight from the searcher's matches in them; needs
    /// [`Self::formats_bytes`]. `how` says whether the search also looks for NUL bytes and
    /// gives the line feeds of `bytes` if they are counted already.
    ///
    /// # Returns
    ///
    /// Whether a line was selected, and how many lines `bytes` holds when lines are numbered.
    /// `None` if NUL bytes are looked for and `bytes` hold one: `out` then holds output to
    /// discard.
    fn format_bytes(
        &self,
        name: &[u8],
        bytes: &[u8],
        (first_line, how): (usize, LineVisit),
        out: &mut Vec<u8>,
    ) -> Option<(bool, usize)> {
        let numbers = self.printer.line_number;
        let mut lines = LineWriter {
            files: self,
            name,
            bytes,
            first_line,
            run: 0..0,
            out,
        };
        let (invert, whole_line) = (self.selection.invert, self.selection.whole_line);
        let mut selected = false;
        // The start and index of the line after the last one selected by matches, whose
        // lines print when inverting.
        let mut unmatched = (0, 0);
        // Lines may come without their matches when nothing printed shows them.
        let with_matches = whole_line
            || self.mode == Mode::OnlyMatching
            || self.printer.color
            || self.printer.column;
        let mut visit = |line: MatchedLine| {
            let whole: SmallVec<[Match; 1]>;
            let matches = if whole_line {
                let len = terminated_line_content_end(bytes, line.start, line.end) - line.start;
                let spans = line.matches.iter().filter(|found| spans_line(found, len));
                whole = spans.copied().collect();
                if whole.is_empty() {
                    return;
                }
                &whole[..]
            } else {
                line.matches
            };
            if invert {
                selected |= lines.unmatched(unmatched.0..line.start, unmatched.1);
                unmatched = (line.end + 1, line.index + 1);
            } else {
                selected = true;
                lines.matched(&line, matches);
            }
        };
        let how = LineVisit {
            with_matches,
            ..how
        };
        let new_lines = if numbers {
            self.searcher
                .visit_matching_lines::<true>(bytes, how, &mut visit)
        } else {
            self.searcher
                .visit_matching_lines::<false>(bytes, how, &mut visit)
        }?;
        if invert {
            selected |= lines.unmatched(unmatched.0.min(bytes.len())..bytes.len(), unmatched.1);
        }
        lines.flush();
        Some((selected, new_lines))
    }

    /// Formats what scanning a piece found into `out`, apart from the rest of its file,
    /// numbering its lines from `first_line`; needs [`Self::formats_alone`].
    ///
    /// # Returns
    ///
    /// Whether a line was selected, and how many lines the piece read anew when lines are
    /// numbered.
    fn format_alone(
        &self,
        name: &[u8],
        scan: Scan,
        first_line: usize,
        out: &mut Vec<u8>,
    ) -> (bool, usize) {
        // The heading is written once the output is added in order.
        let mut progress = Progress {
            next_line: first_line,
            headed: true,
            ..Progress::default()
        };
        self.apply(name, scan, &mut progress, out);
        (progress.selected > 0, progress.next_line - first_line)
    }

    /// Lines kept before each piece for context.
    fn kept_lines(&self) -> usize {
        if self.lays_out_context() {
            self.context.0
        } else {
            0
        }
    }

    /// Whether selected lines print among lines of context, as ripgrep prints them for lines
    /// and, without `--pretty`, for matches.
    fn lays_out_context(&self) -> bool {
        self.context != (0, 0)
            && (self.mode == Mode::Lines
                || (self.mode == Mode::OnlyMatching && self.pretty.is_none()))
    }

    /// Searches `piece` apart from the rest of its file while the file is `searching`, and
    /// passes what it found to `then`.
    fn scan_piece<T>(
        &self,
        piece: &Piece,
        searching: bool,
        then: impl FnOnce(Option<Scan>) -> T,
    ) -> T {
        if !searching {
            return then(None);
        }
        let bytes = piece.bytes;
        if self.formats_bytes() && self.prints_bytes_as_text(bytes) {
            return then(Some(Scan::Bytes(bytes, piece.feeds)));
        }
        if self.counts_by_stream() {
            // Counted as bytes, like `count_stream`, so replaced bytes never match.
            let count = self.searcher.count_matching_lines_in_bytes(bytes);
            return then(Some(Scan::MatchingLines(count)));
        }
        let text = lossy_text(bytes);
        if self.searches_whole_text() {
            return then(Some(if self.mode == Mode::CountMatches {
                Scan::Matches(self.searcher.count(&text))
            } else {
                Scan::Presence(self.searcher.contains(&text))
            }));
        }

        let mut found = self.searcher.find_lines(&text);
        if self.selection.whole_line {
            found.retain_mut(|line| {
                let len = line.line.len();
                line.matches.retain(|span| spans_line(span, len));
                !line.matches.is_empty()
            });
        }
        let prints_raw_lines =
            self.mode == Mode::Lines || (self.mode == Mode::OnlyMatching && self.printer.column);
        let raw = RawLines::new(
            bytes,
            self.selection.invert
                || self.lays_out_context()
                // Text that does not share the bytes' address had bytes replaced.
                || (prints_raw_lines && !std::ptr::eq(text.as_ptr(), bytes.as_ptr())),
        );
        // Without an index of lines, they are only counted when line numbers print: pieces then
        // number lines from zero, which only line numbers and context would show.
        let new_lines = match raw.len() {
            0 if self.printer.line_number => count_lines(piece.new_bytes()),
            0 => 0,
            indexed => indexed - piece.kept_lines,
        };
        let selected = self.select(&found, piece.kept_lines, raw.len());
        then(Some(Scan::Lines {
            selected,
            raw,
            kept: piece.kept_lines,
            new_lines,
        }))
    }

    /// Adds what searching a piece found to its file's `progress`, writing its selected lines
    /// to `out`.
    ///
    /// # Returns
    ///
    /// Whether later pieces can still change the output.
    fn apply(&self, path: &[u8], scan: Scan, progress: &mut Progress, out: &mut Vec<u8>) -> bool {
        let wants_more = |progress: &Progress| {
            self.selection
                .max_count
                .is_none_or(|max| progress.selected < max)
        };
        let (mut selected, raw, kept, new_lines) = match scan {
            Scan::MatchingLines(count) => {
                progress.selected += count;
                return wants_more(progress);
            }
            Scan::Matches(count) => {
                progress.matches += count;
                return true;
            }
            Scan::Presence(found) => {
                progress.selected += usize::from(found);
                return !found;
            }
            Scan::Bytes(bytes, feeds) => {
                // The heading goes first, and back out if no line is selected.
                let (len, headed) = (out.len(), progress.headed);
                self.write_heading(path, progress, out);
                let how = LineVisit {
                    feeds,
                    ..LineVisit::default()
                };
                let (selected, new_lines) = self
                    .format_bytes(path, bytes, (progress.next_line, how), out)
                    .expect("only a search that looks for NUL bytes stops at one");
                if !selected {
                    out.truncate(len);
                    progress.headed = headed;
                }
                progress.selected += usize::from(selected);
                progress.next_line += new_lines;
                return wants_more(progress);
            }
            Scan::Formatted {
                output,
                selected,
                new_lines,
            } => {
                progress.selected += usize::from(selected);
                progress.next_line += new_lines;
                // Selected lines head the file even with no match to print, as when inverted.
                if selected {
                    self.write_heading(path, progress, out);
                }
                // Swapping buffers passes the output on without copying it.
                if !out.is_empty() {
                    out.append(output);
                } else if !output.is_empty() {
                    mem::swap(out, output);
                }
                return wants_more(progress);
            }
            Scan::Lines {
                selected,
                raw,
                kept,
                new_lines,
            } => (selected, raw, kept, new_lines),
        };
        let first_line = progress.next_line - kept;
        progress.next_line += new_lines;
        for line in &mut selected {
            line.index += first_line;
        }
        // Lines past the cap still print as selected among the context owed after it.
        let past_max = match self.selection.max_count {
            Some(max_count) => {
                selected.split_off((max_count - progress.selected).min(selected.len()))
            }
            None => Vec::new(),
        };
        progress.selected += selected.len();
        if let Some(tally) = self.tally {
            let patterns = selected.iter().flat_map(|line| line.matches);
            tally.found(selected.len(), patterns.map(|span| span.pattern_index));
        }

        match self.mode {
            Mode::Quiet | Mode::FilesWithMatches | Mode::FilesWithoutMatch => {
                progress.selected == 0
            }
            Mode::Count => wants_more(progress),
            Mode::CountMatches => {
                progress.matches += if self.selection.invert {
                    selected.len()
                } else {
                    selected.iter().map(|line| line.matches.len()).sum()
                };
                wants_more(progress)
            }
            Mode::OnlyMatching if !self.lays_out_context() => {
                self.write_only_matching(path, &selected, (&raw, first_line), progress, out);
                wants_more(progress)
            }
            Mode::Lines | Mode::OnlyMatching => {
                let lines = first_line..progress.next_line;
                let slots = progress.layout.place(&selected, self.context, lines);
                let lines = Laid {
                    selected: &selected,
                    past_max: &past_max,
                    slots: &slots,
                };
                self.write_lines(path, &lines, (&raw, first_line), progress, out);
                wants_more(progress) || progress.layout.owes_context()
            }
        }
    }

    /// Writes the count or presence a file's pieces add up to.
    ///
    /// # Returns
    ///
    /// Whether the file counts as a success for the exit status.
    fn finish(&self, path: &[u8], progress: &Progress, out: &mut Vec<u8>) -> bool {
        match self.mode {
            Mode::Quiet | Mode::FilesWithMatches | Mode::FilesWithoutMatch => {
                self.write_presence(path, progress.selected > 0, out)
            }
            Mode::Count => self.write_count(path, self.capped(progress.selected), out),
            Mode::CountMatches => self.write_count(path, progress.matches, out),
            Mode::Lines | Mode::OnlyMatching => progress.selected > 0,
        }
    }

    /// The lines of a piece selected by `found` matches, or of its `line_count` lines the
    /// others when inverting, past the `kept` lines an earlier piece searched. Indexes count
    /// from the piece's first line.
    fn select<'a>(
        &self,
        found: &'a [LineMatch<'a>],
        kept: usize,
        line_count: usize,
    ) -> Vec<Selected<'a>> {
        if !self.selection.invert {
            return found
                .iter()
                .filter(|line| line.line_number > kept)
                .map(|line| Selected {
                    index: line.line_number - 1,
                    line: Some(line.line),
                    matches: &line.matches,
                })
                .collect();
        }
        let mut matched = found.iter().map(|line| line.line_number - 1).peekable();
        (kept..line_count)
            .filter(|index| {
                while matched.next_if(|matched| matched < index).is_some() {}
                matched.next_if_eq(index).is_none()
            })
            .map(|index| Selected {
                index,
                line: None,
                matches: &[],
            })
            .collect()
    }

    /// Writes the file's heading before its first output.
    fn write_heading(&self, path: &[u8], progress: &mut Progress, out: &mut Vec<u8>) {
        if !progress.headed {
            progress.headed = true;
            self.printer.heading(out, path);
        }
    }

    /// Writes each match of `selected` lines, read from `raw` lines that start with line
    /// `first_line` of the file.
    fn write_only_matching(
        &self,
        path: &[u8],
        selected: &[Selected],
        (raw, first_line): (&RawLines, usize),
        progress: &mut Progress,
        out: &mut Vec<u8>,
    ) {
        if selected.is_empty() {
            return;
        }
        if let Some(pretty) = self.pretty {
            let rows: Vec<(usize, &str, &[Match])> = (selected.iter())
                .map(|line| (line.index + 1, line.line.unwrap_or_default(), line.matches))
                .collect();
            pretty.only_matches(out, path, &rows);
            return;
        }
        self.write_heading(path, progress, out);
        for line in selected {
            let bytes = if self.printer.column || line.matches.is_empty() {
                raw.get(line.index - first_line, line.line)
            } else {
                &[]
            };
            self.write_matches(path, line, bytes, out);
        }
    }

    /// Writes each match of a selected `line`, or the whole line when inverting selects it,
    /// as ripgrep does. Its bytes are `bytes` when columns print or it has no match.
    fn write_matches(&self, path: &[u8], line: &Selected, bytes: &[u8], out: &mut Vec<u8>) {
        if line.matches.is_empty() {
            let number = line.index + 1;
            return (self.printer).line(out, path, number, bytes, &[], MATCH_SEPARATOR);
        }
        let searched = line.line.unwrap_or_default().as_bytes();
        for span in line.matches {
            // Columns count the file's bytes, which replaced bytes may shift.
            let column = if self.printer.column {
                aligned_matches(searched, bytes, std::slice::from_ref(span))
                    .first()
                    .map_or(span.start, |aligned| aligned.start)
            } else {
                span.start
            };
            let text = &searched[span.start..span.end];
            self.printer
                .only_match(out, path, line.index + 1, column + 1, text);
        }
    }

    /// Writes `laid` lines, read from `raw` lines that start with line `first_line` of the
    /// file.
    fn write_lines(
        &self,
        path: &[u8],
        laid: &Laid,
        (raw, first_line): (&RawLines, usize),
        progress: &mut Progress,
        out: &mut Vec<u8>,
    ) {
        let Laid {
            selected,
            past_max,
            slots,
        } = *laid;
        if slots.is_empty() {
            return;
        }
        let raw_line = |index: usize, line: Option<&str>| raw.get(index - first_line, line);
        // Each slot's selected line, or the index of its context line; `None` for a gap.
        let shown = |slot: &Slot| match *slot {
            Slot::Gap => None,
            Slot::Selected(position) => Some(Ok(&selected[position])),
            Slot::Context(index) => {
                let found = past_max.binary_search_by_key(&index, |line| line.index);
                Some(found.map(|position| &past_max[position]).map_err(|_| index))
            }
        };
        if let Some(pretty) = self.pretty {
            // Pretty rows show text, so a CRLF line's `\r` is dropped instead of drawn.
            let text_of = |index: usize| {
                let bytes = raw_line(index, None);
                String::from_utf8_lossy(bytes.strip_suffix(b"\r").unwrap_or(bytes))
            };
            let texts: Vec<Cow<str>> = (slots.iter())
                .map(|slot| match shown(slot) {
                    None => Cow::Borrowed(""),
                    Some(Err(index)) => text_of(index),
                    Some(Ok(line)) => line.line.map_or_else(|| text_of(line.index), Cow::Borrowed),
                })
                .collect();
            let rows: Vec<Row> = (slots.iter().zip(&texts))
                .map(|(slot, text)| match shown(slot) {
                    None => Row::Gap,
                    Some(Err(index)) => Row::Context(index + 1, text),
                    Some(Ok(line)) => Row::Selected(line.index + 1, text, line.matches),
                })
                .collect();
            pretty.lines(out, path, &rows);
            return;
        }
        self.write_heading(path, progress, out);
        for slot in slots {
            match shown(slot) {
                None => self.printer.context_break(out),
                Some(Err(index)) => {
                    let bytes = raw_line(index, None);
                    self.printer
                        .line(out, path, index + 1, bytes, &[], CONTEXT_SEPARATOR);
                }
                Some(Ok(line)) if self.mode == Mode::OnlyMatching => {
                    self.write_matches(path, line, raw_line(line.index, line.line), out);
                }
                Some(Ok(line)) => {
                    let bytes = raw_line(line.index, line.line);
                    let highlights = line.line.map_or(Cow::Borrowed(&[][..]), |searched| {
                        aligned_matches(searched.as_bytes(), bytes, line.matches)
                    });
                    self.printer.line(
                        out,
                        path,
                        line.index + 1,
                        bytes,
                        &highlights,
                        MATCH_SEPARATOR,
                    );
                }
            }
        }
    }

    /// Searches a structured document; each match prints as `path:JQ_PATH:text`.
    fn search_structured(
        &self,
        searcher: &Searcher,
        path: &[u8],
        text: &str,
        out: &mut Vec<u8>,
    ) -> Result<bool> {
        let mut found = searcher.find_structured(text)?;
        if let Some(max_count) = self.selection.max_count {
            found.truncate(max_count);
        }
        if let Some(tally) = self.tally {
            tally.found(found.len(), found.iter().map(|item| item.pattern_index));
        }
        match self.mode {
            Mode::Quiet | Mode::FilesWithMatches | Mode::FilesWithoutMatch => {
                Ok(self.write_presence(path, !found.is_empty(), out))
            }
            Mode::Count | Mode::CountMatches => Ok(self.write_count(path, found.len(), out)),
            Mode::Lines | Mode::OnlyMatching if found.is_empty() => Ok(false),
            Mode::Lines | Mode::OnlyMatching => {
                if let Some(pretty) = self.pretty {
                    pretty.structured(out, path, text, &found);
                    return Ok(true);
                }
                self.printer.heading(out, path);
                for item in &found {
                    self.printer
                        .structured(out, path, &item.path, item.node_text);
                }
                Ok(!found.is_empty())
            }
        }
    }

    /// Caps a count of selected lines at `--max-count`.
    fn capped(&self, count: usize) -> usize {
        self.selection
            .max_count
            .map_or(count, |max_count| count.min(max_count))
    }

    /// Writes a count; zero counts print only with `--include-zero`, as in ripgrep.
    fn write_count(&self, path: &[u8], count: usize, out: &mut Vec<u8>) -> bool {
        if count > 0 || self.include_zero {
            match self.pretty {
                // Structured counts are always of matches, as documents have no lines.
                Some(pretty) => pretty.count(
                    out,
                    path,
                    count,
                    self.mode == Mode::CountMatches || self.structured,
                ),
                None => self.printer.count(out, path, count),
            }
        }
        count > 0
    }

    /// Writes the path for `-l` or `--files-without-match` and returns the file's success.
    fn write_presence(&self, path: &[u8], found: bool, out: &mut Vec<u8>) -> bool {
        let success = match self.mode {
            Mode::FilesWithoutMatch => !found,
            Mode::Lines
            | Mode::OnlyMatching
            | Mode::Count
            | Mode::CountMatches
            | Mode::FilesWithMatches
            | Mode::Quiet => found,
        };
        if success && self.mode != Mode::Quiet {
            match self.pretty {
                Some(pretty) => pretty.file(out, path),
                None => self.printer.path(out, path),
            }
        }
        success
    }
}

/// `bytes` as text, with bytes that are not UTF-8 replaced. Validating with SIMD first keeps
/// text that is already UTF-8, nearly all of it, from the slower lossy decoder.
fn lossy_text(bytes: &[u8]) -> Cow<'_, str> {
    match simdutf8::basic::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => String::from_utf8_lossy(bytes),
    }
}

/// Whether `found` spans all `len` bytes of its line's text, as `--line-regexp` selects.
fn spans_line(found: &Match, len: usize) -> bool {
    found.start == 0 && found.end == len
}

/// `matches`, offsets into `searched`, moved onto `raw`: the same line before invalid UTF-8
/// was replaced. A match keeps its highlight where all text before or all text after it is
/// unchanged.
fn aligned_matches<'m>(searched: &[u8], raw: &[u8], matches: &'m [Match]) -> Cow<'m, [Match]> {
    if raw.starts_with(searched) {
        return Cow::Borrowed(matches);
    }
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let same_prefix = searched.iter().zip(raw).take_while(|(a, b)| a == b).count();
    let same_suffix = (searched.iter().rev())
        .zip(raw.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let suffix_start = searched.len() - same_suffix;
    Cow::Owned(
        matches
            .iter()
            .filter_map(|span| {
                if span.end <= same_prefix {
                    Some(*span)
                } else if span.start >= suffix_start {
                    // `raw.len() >= same_suffix`, so neither subtraction underflows.
                    Some(Match {
                        start: span.start + raw.len() - searched.len(),
                        end: span.end + raw.len() - searched.len(),
                        pattern_index: span.pattern_index,
                    })
                } else {
                    None
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use crate::cli::pieces::PIECE_LEN;
    use crate::{CaseMode, SearchConfig, SearchConfigBuilder};

    use super::*;

    const PRINTER: Printer = Printer {
        with_filename: false,
        heading: false,
        line_number: true,
        column: false,
        null: false,
        color: false,
    };
    const SELECTION: Selection = Selection {
        invert: false,
        whole_line: false,
        max_count: None,
        text: false,
        binary: false,
    };
    const TEXT: &[u8] = b"one hit\ntwo\nthree\nfour hit\nfive\nsix\nseven\neight hit\nhit\n";

    /// A plain searcher of files, with `--pretty` off.
    fn file_searcher(
        searcher: &Searcher,
        (mode, selection): (Mode, Selection),
        context: (usize, usize),
        printer: Printer,
    ) -> FileSearcher<'_> {
        FileSearcher {
            searcher,
            selection,
            mode,
            include_zero: false,
            context,
            printer,
            pretty: None,
            tally: None,
            structured: false,
            formats: None,
            line_local: true,
        }
    }

    /// Searches `text` whole, doing with NUL bytes what `binary` says.
    fn search_whole(files: &FileSearcher, text: &[u8], binary: Binary) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let (success, _) = files
            .search(b"t", &mut text.to_vec(), binary, &mut out)
            .expect("search should succeed");
        (out, success)
    }

    fn run(mode: Mode, selection: Selection, context: (usize, usize)) -> (String, bool) {
        let searcher =
            Searcher::new(["hit"], SearchConfig::default()).expect("searcher should compile");
        let files = file_searcher(&searcher, (mode, selection), context, PRINTER);
        let (out, success) = search_whole(&files, TEXT, Binary::Search);
        (
            String::from_utf8(out).expect("output should be UTF-8"),
            success,
        )
    }

    #[test]
    fn lines_print_with_numbers() {
        assert_eq!(
            run(Mode::Lines, SELECTION, (0, 0)),
            (
                "1:one hit\n4:four hit\n8:eight hit\n9:hit\n".to_owned(),
                true
            )
        );
    }

    #[test]
    fn context_merges_overlaps_and_breaks_gaps() {
        let (output, _) = run(Mode::Lines, SELECTION, (1, 1));

        assert_eq!(
            output,
            "1:one hit\n2-two\n3-three\n4:four hit\n5-five\n--\n7-seven\n8:eight hit\n9:hit\n"
        );
    }

    #[test]
    fn inverted_and_capped_selections() {
        let inverted = Selection {
            invert: true,
            max_count: Some(2),
            ..SELECTION
        };

        assert_eq!(run(Mode::Lines, inverted, (0, 0)).0, "2:two\n3:three\n");
        assert_eq!(run(Mode::Count, inverted, (0, 0)).0, "2\n");
    }

    #[test]
    fn whole_line_selection_keeps_exact_lines() {
        let whole = Selection {
            whole_line: true,
            ..SELECTION
        };

        assert_eq!(run(Mode::Lines, whole, (0, 0)).0, "9:hit\n");
    }

    #[test]
    fn counts_and_presence() {
        assert_eq!(run(Mode::Count, SELECTION, (0, 0)).0, "4\n");
        assert_eq!(run(Mode::CountMatches, SELECTION, (0, 0)).0, "4\n");
        assert_eq!(
            run(Mode::FilesWithMatches, SELECTION, (0, 0)),
            ("t\n".to_owned(), true)
        );
        assert_eq!(
            run(Mode::FilesWithoutMatch, SELECTION, (0, 0)),
            (String::new(), false)
        );
        assert_eq!(run(Mode::Quiet, SELECTION, (0, 0)), (String::new(), true));
    }

    #[test]
    fn matches_align_around_replaced_bytes() {
        let span = |start, end| Match {
            start,
            end,
            pattern_index: 0,
        };
        let searched = "caf\u{fffd} ERROR x".as_bytes();
        let raw = b"caf\xe9 ERROR x\r";
        let matches = [span(0, 3), span(7, 12), span(2, 5)];

        let aligned = aligned_matches(searched, raw, &matches);

        assert_eq!(aligned.as_ref(), [span(0, 3), span(5, 10)]);
    }

    #[test]
    fn invalid_utf8_lines_print_as_their_original_bytes() {
        let searcher =
            Searcher::new(["ERROR"], SearchConfig::default()).expect("searcher should compile");
        let files = FileSearcher {
            searcher: &searcher,
            selection: SELECTION,
            mode: Mode::Lines,
            include_zero: false,
            context: (0, 0),
            printer: PRINTER,
            pretty: None,
            tally: None,
            structured: false,
            formats: None,
            line_local: true,
        };
        let (out, _) = search_whole(&files, b"caf\xe9 ERROR\r\nok\n", Binary::Search);

        assert_eq!(out, b"1:caf\xe9 ERROR\r\n");
    }

    #[test]
    fn pretty_lines_drop_carriage_returns_and_count_matches() {
        let searcher =
            Searcher::new(["ERROR"], SearchConfig::default()).expect("searcher should compile");
        let tally = Tally::new(1);
        let files = FileSearcher {
            searcher: &searcher,
            selection: SELECTION,
            mode: Mode::Lines,
            include_zero: false,
            context: (0, 0),
            printer: PRINTER,
            pretty: Some(Pretty {
                depth: crate::cli::style::Depth::None,
                width: None,
                patterns: 1,
                values: None,
            }),
            tally: Some(&tally),
            structured: false,
            formats: None,
            line_local: true,
        };
        let (out, _) = search_whole(&files, b"ok\r\nERROR one ERROR\r\n", Binary::Search);

        assert_eq!(
            String::from_utf8(out).expect("output should be UTF-8"),
            "t  1 line \u{b7} 2 matches\n2 \u{258e} ERROR one ERROR\n"
        );
    }

    /// Lines with matches scattered among CRLF, invalid UTF-8, and lines without one, ending
    /// without a line feed.
    fn long_text() -> Vec<u8> {
        let mut text: Vec<u8> = (0..600)
            .flat_map(|index| match index % 37 {
                0 => format!("{index} hit\n").into_bytes(),
                5 => format!("{index} hit and hit\r\n").into_bytes(),
                11 => b"caf\xe9 hit\n".to_vec(),
                12 => b"hit\n".to_vec(),
                _ => format!("{index} filler\n").into_bytes(),
            })
            .collect();
        text.extend_from_slice(b"last hit");
        text
    }

    /// Searches `text` in pieces of at most `piece_len` bytes, holding all of the output.
    fn search_in_pieces(
        files: &FileSearcher,
        text: &[u8],
        binary: Binary,
        piece_len: usize,
    ) -> (Vec<u8>, bool) {
        let source = Source {
            name: b"t",
            reader: text,
            len: None,
            path: None,
            binary,
            piece_len,
            parallel: false,
        };
        let mut out = Vec::new();
        let success = files
            .search_reader(source, &mut out, None)
            .expect("reads from memory succeed");
        (out, success)
    }

    /// Searches the file at `path` in a pool of three threads, in pieces of at most
    /// `piece_len` bytes, holding all of the output.
    fn search_in_parallel(
        files: &FileSearcher,
        path: &Path,
        binary: Binary,
        piece_len: usize,
    ) -> (Vec<u8>, bool) {
        let file = std::fs::File::open(path).expect("the temporary file opens");
        let source = Source {
            name: b"t",
            len: Some(file.metadata().expect("the file has metadata").len()),
            reader: file,
            path: Some(path),
            binary,
            piece_len,
            parallel: true,
        };
        let mut out = Vec::new();
        let success = on_threads(3, || {
            assert!(source.reads_in_parallel());
            files.search_reader(source, &mut out, None)
        })
        .expect("the file reads");
        (out, success)
    }

    /// Runs `run` in a pool of `threads` threads.
    fn on_threads<T: Send>(threads: usize, run: impl FnOnce() -> T + Send) -> T {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build();
        pool.expect("the pool starts").install(run)
    }

    /// A temporary file holding `content`.
    fn temporary_file(content: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create a temporary file");
        io::Write::write_all(&mut file, content).expect("write the temporary file");
        file
    }

    #[test]
    fn pieces_give_the_output_of_the_whole_text() {
        let searcher =
            Searcher::new(["hit"], SearchConfig::default()).expect("searcher should compile");
        let text = long_text();
        let selections = [
            SELECTION,
            Selection {
                invert: true,
                ..SELECTION
            },
            Selection {
                max_count: Some(5),
                ..SELECTION
            },
            Selection {
                invert: true,
                max_count: Some(40),
                ..SELECTION
            },
            Selection {
                whole_line: true,
                ..SELECTION
            },
            Selection {
                max_count: Some(0),
                ..SELECTION
            },
        ];
        let modes = [
            Mode::Lines,
            Mode::OnlyMatching,
            Mode::Count,
            Mode::CountMatches,
            Mode::FilesWithMatches,
            Mode::FilesWithoutMatch,
            Mode::Quiet,
        ];
        let printers = [
            PRINTER,
            Printer {
                line_number: false,
                ..PRINTER
            },
            Printer {
                column: true,
                heading: true,
                with_filename: true,
                ..PRINTER
            },
        ];
        let file = temporary_file(&text);
        for (mode, selection, context, printer) in combinations_of(&modes, &selections, &printers) {
            let files = file_searcher(&searcher, (mode, selection), context, printer);
            let (whole, success) = search_whole(&files, &text, Binary::Search);
            for piece_len in [1, 60, 700, PIECE_LEN] {
                assert_eq!(
                    search_in_pieces(&files, &text, Binary::Search, piece_len),
                    (whole.clone(), success),
                    "{mode:?} {selection:?} {context:?} {printer:?} in pieces of {piece_len}"
                );
            }
            for piece_len in [7, 60, 700] {
                assert_eq!(
                    search_in_parallel(&files, file.path(), Binary::Search, piece_len),
                    (whole.clone(), success),
                    "{mode:?} {selection:?} {context:?} {printer:?} on threads in pieces of \
                     {piece_len}"
                );
            }
        }
    }

    /// Lines with matches of `hit` and `café` among invalid UTF-8, line feeds after carriage
    /// returns, and a last line without a line feed.
    fn mixed_text() -> Vec<u8> {
        let mut text = Vec::new();
        for index in 0..300 {
            text.extend_from_slice(match index % 7 {
                0 => b"caf\xe9 hit\r\n",
                1 => b"miss\n",
                2 => b"hit\r\n",
                3 => b"\xff\xfe hit hit \x80\n",
                4 => b"hit\n",
                // `café` after a lone lead byte and a cut-off sequence, and before a stray
                // continuation byte.
                5 => b"\xc3caf\xc3\xa9\xa9 \xe2\x82caf\xc3\xa9 hit\n",
                _ => b"plain line\n",
            });
        }
        text.extend_from_slice(b"last hit");
        text
    }

    #[test]
    fn bytes_format_like_their_decoded_text() {
        for pattern in ["hit", "caf\u{e9}", "CAF\u{e9}"] {
            for (case_mode, whole_word) in [
                (CaseMode::Sensitive, false),
                (CaseMode::AsciiInsensitive, false),
                (CaseMode::Sensitive, true),
            ] {
                let config = SearchConfig::builder()
                    .case_mode(case_mode)
                    .whole_word(whole_word);
                bytes_format_like_decoded_text_of(pattern, config);
            }
        }
    }

    /// Checks that searching for `pattern` with `config` in raw bytes prints what searching
    /// decoded text does.
    fn bytes_format_like_decoded_text_of(pattern: &str, config: SearchConfigBuilder) {
        let fused = Searcher::new([pattern], config.build()).expect("searcher should compile");
        // Overlapping matches keep this searcher on decoded text, with the same matches.
        let decoded = Searcher::new([pattern, "absent"], config.overlapping(true).build())
            .expect("searcher should compile");
        assert!(fused.matches_bytes_as_text() && !decoded.matches_bytes_as_text());
        let text = mixed_text();
        let file = temporary_file(&text);
        let selections = [
            SELECTION,
            Selection {
                invert: true,
                ..SELECTION
            },
            Selection {
                whole_line: true,
                ..SELECTION
            },
            Selection {
                whole_line: true,
                invert: true,
                ..SELECTION
            },
        ];
        let bare = Printer {
            line_number: false,
            ..PRINTER
        };
        let printers = [
            PRINTER,
            bare,
            Printer {
                color: true,
                ..bare
            },
            Printer {
                column: true,
                with_filename: true,
                ..PRINTER
            },
            Printer {
                with_filename: true,
                heading: true,
                ..bare
            },
        ];
        for mode in [Mode::Lines, Mode::OnlyMatching] {
            for (&selection, &printer) in selections
                .iter()
                .flat_map(|s| printers.iter().map(move |p| (s, p)))
            {
                let expected = file_searcher(&decoded, (mode, selection), (0, 0), printer);
                let files = file_searcher(&fused, (mode, selection), (0, 0), printer);
                let (whole, success) = search_whole(&expected, &text, Binary::Search);
                for piece_len in [1, 60, PIECE_LEN] {
                    assert_eq!(
                        search_in_pieces(&files, &text, Binary::Search, piece_len),
                        (whole.clone(), success),
                        "{pattern} {mode:?} {selection:?} {printer:?} in pieces of {piece_len}"
                    );
                }
                assert_eq!(
                    search_in_parallel(&files, file.path(), Binary::Search, 60),
                    (whole.clone(), success),
                    "{pattern} {mode:?} {selection:?} {printer:?} on threads"
                );
            }
        }
    }

    #[test]
    fn highlights_and_columns_format_from_bytes_only_when_valid() {
        let searcher =
            Searcher::new(["hit"], SearchConfig::default()).expect("searcher should compile");
        let plain = file_searcher(&searcher, (Mode::Lines, SELECTION), (0, 0), PRINTER);
        let colored = Printer {
            color: true,
            ..PRINTER
        };
        let colored = file_searcher(&searcher, (Mode::Lines, SELECTION), (0, 0), colored);

        assert!(plain.prints_bytes_as_text(b"caf\xe9 hit"));
        assert!(colored.prints_bytes_as_text("caf\u{e9} hit".as_bytes()));
        assert!(!colored.prints_bytes_as_text(b"caf\xe9 hit"));
    }

    #[test]
    fn turns_run_in_order_whichever_thread_leaves_or_takes_them() {
        let (threads, pieces) = (3, 60);
        let turns = Turns::new(Vec::new(), threads);
        let ended = AtomicBool::new(false);
        thread::scope(|scope| {
            for first in 0..threads {
                let (turns, ended) = (&turns, &ended);
                scope.spawn(move || {
                    for index in (first..pieces).step_by(threads) {
                        let turn = move |order: &mut Vec<usize>| {
                            order.push(index);
                            false
                        };
                        if index % 4 == 0 {
                            turns.take(index, ended, turn);
                        } else {
                            turns.leave(index, ended, Box::new(turn));
                        }
                        let passed = turns.read_past(index, ended, Vec::len);
                        assert!(passed.is_some_and(|len| len > index));
                    }
                });
            }
        });

        assert_eq!(turns.into_inner(), (0..pieces).collect::<Vec<_>>());
    }

    #[test]
    fn a_turn_that_ends_the_search_runs_no_later_turn() {
        let turns = Turns::new(0, 1);
        let ended = AtomicBool::new(false);
        let add = |ends| {
            Box::new(move |count: &mut usize| {
                *count += 1;
                ends
            })
        };
        turns.leave(2, &ended, add(false));
        turns.leave(1, &ended, add(true));

        assert!(turns.leave(0, &ended, add(false)));
        assert_eq!(turns.into_inner(), 2);
    }

    /// Every mode, selection, context, and printer together.
    fn combinations_of(
        modes: &[Mode],
        selections: &[Selection],
        printers: &[Printer],
    ) -> Vec<(Mode, Selection, (usize, usize), Printer)> {
        let contexts = [(0, 0), (2, 0), (0, 3), (2, 2)];
        let mut combinations = Vec::new();
        for &mode in modes {
            for &selection in selections {
                for context in contexts {
                    for &printer in printers {
                        combinations.push((mode, selection, context, printer));
                    }
                }
            }
        }
        combinations
    }

    /// Lines with matches, more than ripgrep's first read holds, then a NUL byte's line and
    /// `after` past it; returns them and where the NUL byte's line starts.
    fn text_with_late_nul(after: &[u8]) -> (Vec<u8>, usize) {
        let mut text = long_text().repeat(20);
        text.push(b'\n');
        let line_start = text.len();
        text.extend_from_slice(b"late\0 hit\n");
        text.extend_from_slice(after);
        (text, line_start)
    }

    #[test]
    fn every_search_path_stops_at_the_line_of_a_late_nul_byte() {
        let searcher =
            Searcher::new(["hit"], SearchConfig::default()).expect("searcher should compile");
        let capped = Selection {
            max_count: Some(3),
            ..SELECTION
        };
        let inverted = Selection {
            invert: true,
            ..SELECTION
        };
        let modes = [
            Mode::Lines,
            Mode::OnlyMatching,
            Mode::Count,
            Mode::FilesWithMatches,
            Mode::FilesWithoutMatch,
        ];
        let (text, _) = text_with_late_nul(b"filler\nhit\n");
        let file = temporary_file(&text);
        for (mode, selection, context, printer) in
            combinations_of(&modes, &[SELECTION, capped, inverted], &[PRINTER])
        {
            let files = file_searcher(&searcher, (mode, selection), context, printer);
            for binary in [Binary::Notice, Binary::Quit] {
                let whole = search_whole(&files, &text, binary);
                let case = format!("{mode:?} {selection:?} {context:?} {binary:?}");

                assert_eq!(search_in_pieces(&files, &text, binary, 60), whole, "{case}");
                assert_eq!(
                    search_in_parallel(&files, file.path(), binary, 60),
                    whole,
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn a_nul_byte_ends_the_lines_printed_with_a_notice_or_warning() {
        let searcher =
            Searcher::new(["hit"], SearchConfig::default()).expect("searcher should compile");
        let files = file_searcher(&searcher, (Mode::Lines, SELECTION), (0, 0), PRINTER);
        let counts = file_searcher(&searcher, (Mode::Count, SELECTION), (0, 0), PRINTER);
        let (text, line_start) = text_with_late_nul(b"");
        let (mut notice, _) = search_whole(&files, &text[..line_start], Binary::Search);
        let mut stopped = notice.clone();
        files
            .printer
            .binary_match(&mut notice, b"t", line_start + 4);
        files
            .printer
            .binary_stopped(&mut stopped, b"t", line_start + 4);

        assert_eq!(search_whole(&files, &text, Binary::Notice), (notice, true));
        assert_eq!(search_whole(&files, &text, Binary::Quit), (stopped, true));
        assert_eq!(
            search_whole(&counts, &text, Binary::Quit),
            (Vec::new(), false)
        );
        // A NUL byte in ripgrep's first read prints only the notice.
        let mut early = Vec::new();
        files.printer.binary_match(&mut early, b"t", 5);
        assert_eq!(
            search_whole(&files, b"hit\nx\0 hit\n", Binary::Notice),
            (early, true)
        );
        assert_eq!(
            search_whole(&files, b"hit\nx\0 hit\n", Binary::Quit),
            (Vec::new(), false)
        );
        // NUL bytes past the notice's line end lines.
        let mut past = Vec::new();
        files.printer.binary_match(&mut past, b"t", 1);
        assert_eq!(
            search_whole(&files, b"x\0hit\n", Binary::Notice),
            (past, true)
        );
    }

    #[test]
    fn context_past_a_nul_byte_ends_the_search_as_in_ripgrep() {
        let searcher =
            Searcher::new(["hit"], SearchConfig::default()).expect("searcher should compile");
        let after = file_searcher(&searcher, (Mode::Lines, SELECTION), (0, 1), PRINTER);
        let before = file_searcher(&searcher, (Mode::Lines, SELECTION), (1, 0), PRINTER);
        let notice = |prefix: &[u8], nul_offset| {
            let mut out = prefix.to_vec();
            PRINTER.binary_match(&mut out, b"t", nul_offset);
            (out, true)
        };

        // A line selected past the NUL byte ends the group of lines before it.
        assert_eq!(
            search_whole(&after, b"hit\nx\0\nhit\n", Binary::Notice),
            notice(b"", 5)
        );
        let filler = b"filler\n".repeat(10_000);
        let text = [b"hit\n", &filler[..], b"x\0\nhit\n"].concat();
        assert_eq!(
            search_whole(&after, &text, Binary::Notice),
            notice(b"1:hit\n2-filler\n--\n", 5 + filler.len())
        );
        // Context still owed at the NUL byte's line ends the search there.
        let owed = [&filler[..], b"hit\nx\0\nhit\n"].concat();
        assert_eq!(
            search_whole(&after, &owed, Binary::Notice),
            notice(format!("{}:hit\n", 10_001).as_bytes(), filler.len() + 5)
        );
        // With context before it, a line selected past the NUL byte follows a context line,
        // unless it is the first.
        assert_eq!(
            search_whole(&before, b"x\0y\nhit\n", Binary::Notice),
            (Vec::new(), false)
        );
        assert_eq!(
            search_whole(&before, b"x\0hit\n", Binary::Notice),
            (Vec::new(), false)
        );
        assert_eq!(
            search_whole(&before, b"\0hit\n", Binary::Notice),
            (Vec::new(), false)
        );
        assert_eq!(
            search_whole(&before, b"hit x\0\n", Binary::Notice),
            notice(b"", 5)
        );
    }

    /// Searches the file at `path` in a pool of `threads` threads, with output emitted as it is
    /// found.
    fn search_emitting(
        files: &FileSearcher,
        path: &Path,
        binary: Binary,
        threads: usize,
    ) -> (Vec<u8>, usize, bool) {
        let file = std::fs::File::open(path).expect("the temporary file opens");
        let len = file.metadata().expect("the file has metadata").len();
        let source = Source {
            name: b"t",
            reader: file,
            len: Some(len),
            path: Some(path),
            binary,
            piece_len: 64 * 1024,
            parallel: true,
        };
        let (mut emitted, mut writes) = (Vec::new(), 0);
        let mut emit = |bytes: &[u8]| {
            writes += usize::from(!bytes.is_empty());
            emitted.extend_from_slice(bytes);
            Ok(())
        };
        let mut held = Vec::new();
        let success = on_threads(threads, || {
            assert_eq!(source.reads_in_parallel(), threads > 1);
            files.search_reader(source, &mut held, Some(&mut emit))
        })
        .expect("the file reads");
        assert!(held.is_empty(), "emitted searches hold no output");
        (emitted, writes, success)
    }

    #[test]
    fn output_before_a_late_nul_byte_streams_as_it_is_found() {
        let searcher =
            Searcher::new(["hit"], SearchConfig::default()).expect("searcher should compile");
        let files = file_searcher(&searcher, (Mode::Lines, SELECTION), (0, 0), PRINTER);
        let lines = b"hit and some filler text\n".repeat(40_000);
        let binary_file = temporary_file(&[&lines[..], b"\0\n"].concat());
        let (mut expected, _) = search_whole(&files, &lines, Binary::Search);
        PRINTER.binary_match(&mut expected, b"t", lines.len());

        for threads in [1, 3] {
            let (emitted, writes, success) =
                search_emitting(&files, binary_file.path(), Binary::Notice, threads);

            assert_eq!((&emitted, success), (&expected, true));
            assert!(writes > 1, "output was written as it was found");
        }
    }

    #[test]
    fn only_matching_prints_each_match() {
        assert_eq!(
            run(Mode::OnlyMatching, SELECTION, (0, 0)).0,
            "1:hit\n4:hit\n8:hit\n9:hit\n"
        );
    }

    #[test]
    fn only_matching_prints_context_and_inverted_lines_as_ripgrep_does() {
        let inverted = Selection {
            invert: true,
            ..SELECTION
        };

        assert_eq!(
            run(Mode::OnlyMatching, SELECTION, (0, 1)).0,
            "1:hit\n2-two\n--\n4:hit\n5-five\n--\n8:hit\n9:hit\n"
        );
        assert_eq!(
            run(Mode::OnlyMatching, inverted, (0, 0)).0,
            "2:two\n3:three\n5:five\n6:six\n7:seven\n"
        );
    }

    #[test]
    fn lines_past_the_cap_print_as_selected_within_the_context_owed() {
        let capped = Selection {
            max_count: Some(1),
            ..SELECTION
        };
        let dense = b"hit 1\nx\nhit 2\nhit 3\nhit 4\n";
        let searcher =
            Searcher::new(["hit"], SearchConfig::default()).expect("searcher should compile");
        let files = file_searcher(&searcher, (Mode::Lines, capped), (0, 3), PRINTER);

        assert_eq!(
            search_whole(&files, dense, Binary::Search),
            (b"1:hit 1\n2-x\n3:hit 2\n4:hit 3\n".to_vec(), true)
        );
        assert_eq!(
            search_in_pieces(&files, dense, Binary::Search, 1),
            search_whole(&files, dense, Binary::Search)
        );
    }
}
