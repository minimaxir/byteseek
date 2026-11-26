use std::num::NonZeroUsize;

use memchr::{memchr_iter, memrchr};
use smallvec::SmallVec;

#[cfg(feature = "cli")]
use crate::Watch;
use crate::approx::{
    ApproximateEngine, ApproximateLinePlan, count_matching_lines_from_global_matches,
};
use crate::boolean_query::{BooleanLinePlan, BooleanQueryEngine};
use crate::chunks::{adopt_first_chunk, map_chunks, merge_chunks, release_unused_capacity};
use crate::exact::{BlockPlan, Extent, LineCountPlan, MultiPatternEngine, PairPlan, PairScanner};
use crate::matcher::terminated_line_content_end;
use crate::{LineMatch, Match, Watched, Watcher, effective_limit, line_end_from, profiles};

use super::parallel::line_split_points;
use super::{Engine, Searcher};

/// Gap in bytes beyond which the collector skips lines in bulk.
const LINE_JUMP_MIN_GAP: usize = 256;

/// Whether a line search of `haystack_len` bytes is worth splitting across threads, taking
/// its lines to be of ordinary length.
fn parallel_lines_pay_off(haystack_len: usize) -> bool {
    /// Roughly the length of a line of logs or source code.
    const ASSUMED_LINE_LEN: usize = 80;

    profiles::use_parallel_lines(haystack_len, haystack_len / ASSUMED_LINE_LEN)
}

/// Whether a line search covers a whole input, given as its text, or one parallel chunk of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineScope<'t> {
    Input(&'t str),
    Chunk,
}

/// How [`Searcher::visit_matching_lines`] goes over bytes.
#[cfg(feature = "cli")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LineVisit {
    /// Lines come with their matches.
    pub(crate) with_matches: bool,
    /// The bytes may hold NUL bytes, which end the visit.
    pub(crate) nuls: bool,
    /// The line feeds of the bytes, when they were counted before the visit.
    pub(crate) feeds: Option<usize>,
}

/// A line holding matches, as [`MatchedLines`] streams it.
#[cfg(feature = "cli")]
#[derive(Debug)]
pub(crate) struct MatchedLine<'m> {
    /// Offset of the line's first byte.
    pub(crate) start: usize,
    /// Offset of the line feed ending the line, or of the end of the input.
    pub(crate) end: usize,
    /// Matches by offset from `start`, in order, none running past `end`; none when the
    /// caller did not ask for them and the line was found without them.
    pub(crate) matches: &'m [Match],
    /// Lines before this one, when lines are counted.
    pub(crate) index: usize,
}

/// The line of `bytes` that matches, given in start order, start on: lines split at line
/// feeds, numbered when `COUNT`. A match starting on a line feed or past the last line, or
/// running past its line's end, lies on no line.
struct LineCursor<'b, const COUNT: bool> {
    bytes: &'b [u8],
    /// The current line's start and end, as in [`MatchedLine`].
    start: usize,
    end: usize,
    /// Lines before the current one, when counted.
    index: usize,
    /// An offset with the line feeds before it, counted by whatever found the matches.
    counted: (usize, usize),
}

impl<'b, const COUNT: bool> LineCursor<'b, COUNT> {
    fn new(bytes: &'b [u8]) -> Self {
        Self {
            bytes,
            start: 0,
            end: line_end_from(bytes, 0),
            index: 0,
            counted: (0, 0),
        }
    }

    /// Takes the line feeds counted in the bytes `watched` by whatever finds the matches, so
    /// the lines between matches are not counted again. The count starts in the line last
    /// moved to, the current line here, or covers all the bytes if there was none. The same
    /// count may come again once the cursor has moved on, so it is taken once: a count is
    /// told from the one taken last by the offset it reaches, and one that reaches the same
    /// offset is passed over, which only leaves the cursor a farther offset to count from.
    #[cfg(feature = "cli")]
    #[inline]
    fn note(&mut self, watched: Watched) {
        if COUNT && watched.len != self.counted.0 {
            self.counted = (watched.len, self.index + watched.feeds);
        }
    }

    /// The line feeds before offset `at`, counted from the nearer of `own`, an offset at or
    /// before `at` with the line feeds before it, and the offset counted elsewhere.
    #[inline]
    fn feeds_before(&self, at: usize, own: (usize, usize)) -> usize {
        let counted = self.counted.0;
        // Where lines are found often, the count elsewhere starts over at each and so lies
        // behind `own`, which the first test tells.
        if counted <= own.0 || counted.abs_diff(at) >= at - own.0 {
            own.1 + memchr_iter(b'\n', &self.bytes[own.0..at]).count()
        } else {
            self.feeds_from_counted(at)
        }
    }

    /// The line feeds before offset `at`, counted from the offset counted elsewhere.
    #[inline(never)]
    fn feeds_from_counted(&self, at: usize) -> usize {
        let (from, feeds) = self.counted;
        if from <= at {
            feeds + memchr_iter(b'\n', &self.bytes[from..at]).count()
        } else {
            feeds - memchr_iter(b'\n', &self.bytes[at..from]).count()
        }
    }

    /// Whether offset `at` lies past the current line.
    #[inline]
    fn passed(&self, at: usize) -> bool {
        at >= self.end
    }

    /// Moves to the line holding offset `at`, which lies past the current line.
    #[inline]
    fn advance_to(&mut self, at: usize) {
        // Across a long gap, jump to the last line feed before `at`, counting the skipped
        // ones in one pass.
        let gap_start = self.end + 1;
        if at > gap_start + LINE_JUMP_MIN_GAP
            && let Some(offset) = memrchr(b'\n', &self.bytes[gap_start..at])
        {
            let last_newline = gap_start + offset;
            if COUNT {
                // The current line's own line feed lies just before the gap.
                self.index = self.feeds_before(last_newline, (gap_start, self.index + 1));
            }
            self.end = last_newline;
        }
        while at >= self.end && self.end < self.bytes.len() {
            self.start = self.end + 1;
            self.end = line_end_from(self.bytes, self.start);
            self.index += usize::from(COUNT);
        }
    }

    /// Moves to the line `found` starts on, if that is past the current one.
    ///
    /// # Returns
    ///
    /// Whether `found` lies on the line.
    #[inline]
    fn seek(&mut self, found: &Match) -> bool {
        if self.passed(found.start) {
            self.advance_to(found.start);
        }
        found.start >= self.start && found.start < self.end && found.end <= self.end
    }

    /// The match `found`, on the current line, by offset from its start.
    #[inline]
    fn relative(&self, found: &Match) -> Match {
        Match {
            start: found.start - self.start,
            end: found.end - self.start,
            pattern_index: found.pattern_index,
        }
    }

    /// The lines of `bytes`, as `str::lines` splits them, when `COUNT`; zero otherwise.
    fn line_count(&self) -> usize {
        if COUNT {
            let unended = self.bytes.last().is_some_and(|&byte| byte != b'\n');
            self.feeds_before(self.bytes.len(), (self.start, self.index)) + usize::from(unended)
        } else {
            0
        }
    }
}

/// Streams the lines that matches, given in start order, start on, each with its matches.
#[cfg(feature = "cli")]
struct MatchedLines<'b, const COUNT: bool> {
    lines: LineCursor<'b, COUNT>,
    /// The current line's matches.
    matches: SmallVec<[Match; 4]>,
}

#[cfg(feature = "cli")]
impl<'b, const COUNT: bool> MatchedLines<'b, COUNT> {
    fn new(bytes: &'b [u8]) -> Self {
        Self {
            lines: LineCursor::new(bytes),
            matches: SmallVec::new(),
        }
    }

    /// Adds `found`, first streaming the current line to `visit` if `found` starts past it.
    #[inline]
    fn record(&mut self, found: Match, visit: &mut impl FnMut(MatchedLine)) {
        if self.lines.passed(found.start) {
            self.flush(visit);
        }
        if self.lines.seek(&found) {
            self.matches.push(self.lines.relative(&found));
        }
    }

    /// Streams the line holding offset `at`, past every line streamed so far, to `visit`
    /// without its matches.
    #[inline]
    fn record_line(&mut self, at: usize, visit: &mut impl FnMut(MatchedLine)) {
        if self.lines.passed(at) {
            self.lines.advance_to(at);
        }
        visit(MatchedLine {
            start: self.lines.start,
            end: self.lines.end,
            matches: &[],
            index: self.lines.index,
        });
    }

    /// Streams the current line to `visit` if it holds matches.
    fn flush(&mut self, visit: &mut impl FnMut(MatchedLine)) {
        if self.matches.is_empty() {
            return;
        }
        visit(MatchedLine {
            start: self.lines.start,
            end: self.lines.end,
            matches: &self.matches,
            index: self.lines.index,
        });
        self.matches.clear();
    }

    /// Streams the last line to `visit` if it holds matches.
    ///
    /// # Returns
    ///
    /// The lines of the input, when `COUNT`; zero otherwise.
    fn finish(mut self, visit: &mut impl FnMut(MatchedLine)) -> usize {
        self.flush(visit);
        self.lines.line_count()
    }
}

/// Collects the matching lines of a text as [`LineMatch`]es, numbered from one.
struct LineMatchCollector<'a> {
    haystack: &'a str,
    lines: LineCursor<'a, true>,
    results: Vec<LineMatch<'a>>,
    /// Lines reserved up front from an estimate, or zero when the buffer grows on demand.
    reserved_lines: usize,
}

impl<'a> LineMatchCollector<'a> {
    fn new(haystack: &'a str, expected_lines: usize) -> Self {
        Self {
            haystack,
            lines: LineCursor::new(haystack.as_bytes()),
            results: Vec::with_capacity(expected_lines),
            reserved_lines: expected_lines,
        }
    }

    fn record(&mut self, candidate: Match) {
        if !self.lines.seek(&candidate) {
            return;
        }
        // A match may reach into the CR stripped from a CRLF line, so clamp it to the line.
        let relative = self.lines.relative(&candidate);
        let clamped = |len: usize| Match {
            end: relative.end.min(len),
            ..relative
        };
        let line_number = self.lines.index + 1;
        if let Some(line) = (self.results.last_mut()).filter(|line| line.line_number == line_number)
        {
            line.matches.push(clamped(line.line.len()));
            return;
        }
        let (start, end) = (self.lines.start, self.lines.end);
        let content_end = terminated_line_content_end(self.haystack.as_bytes(), start, end);
        let mut matches = SmallVec::new();
        matches.push(clamped(content_end - start));
        self.results.push(LineMatch {
            line_number,
            line: &self.haystack[start..content_end],
            matches,
        });
    }

    /// Returns the collected lines, shrinking a reserve that was mostly unused.
    fn finish_lines(mut self) -> Vec<LineMatch<'a>> {
        if self.reserved_lines != 0 && self.results.capacity() / 2 > self.results.len() {
            self.results.shrink_to_fit();
        }
        self.results
    }

    /// Like [`Self::finish_lines`], also returning the haystack's line count.
    fn finish_with_line_count(self) -> (Vec<LineMatch<'a>>, usize) {
        let line_count = self.lines.line_count();
        (self.finish_lines(), line_count)
    }
}

impl Searcher {
    /// Counts lines that contain at least one match.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text split by `str::lines`.
    ///
    /// # Returns
    ///
    /// Number of lines with at least one accepted match.
    #[must_use]
    pub fn count_matching_lines(&self, haystack: &str) -> usize {
        if haystack.is_empty() {
            return 0;
        }

        if matches!(self.engine, Engine::Structured(_)) {
            return count_matching_lines_from_global_matches(
                haystack.as_bytes(),
                self.find_with_limit(haystack, None),
            );
        }

        let bytes = haystack.as_bytes();
        if self.searches_lines_one_by_one() {
            return self.count_lines_one_by_one(bytes);
        }

        if matches!(self.engine, Engine::Approximate(_)) {
            // The engine splits its count over the pool's threads by itself.
            return PlannedLineCounter::new(self, bytes, Extent::ON_ONE_THREAD).count(bytes);
        }
        if parallel_lines_pay_off(bytes.len()) {
            return self.count_matching_lines_parallel(bytes);
        }

        if self.line_pairs.is_some() {
            return PlannedLineCounter::new(self, bytes, Extent::ON_ONE_THREAD).count(bytes);
        }
        self.count_matching_lines_by_engine(bytes)
    }

    /// Counts matching lines with the engine's own unplanned search.
    fn count_matching_lines_by_engine(&self, haystack: &[u8]) -> usize {
        self.visit_matching_lines_by_engine(haystack, |_| {})
    }

    /// Counts matching lines like [`Self::count_matching_lines_by_engine`], passing `on_line`
    /// an offset in each, in order, for the exact engines only.
    fn visit_matching_lines_by_engine(&self, haystack: &[u8], on_line: impl FnMut(usize)) -> usize {
        match &self.engine {
            Engine::Single(engine) => engine.visit_matching_lines(haystack, self.config, on_line),
            Engine::Multi(engine) => {
                engine.visit_matching_lines(haystack, self.config, &self.pattern_lengths, on_line)
            }
            Engine::Boolean(engine) => engine.count_matching_lines(haystack, self.config),
            // Counts what `find_lines` reports: a window spanning lines matches none of them
            // but must not hide matches within them.
            Engine::Approximate(engine) => engine.count_searched_lines(haystack, self.config),
            Engine::Structured(_) => count_matching_lines_from_global_matches(
                haystack,
                self.find_sequential(haystack, usize::MAX),
            ),
        }
    }

    /// Finds matches on each line of `haystack`.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text split by `str::lines`.
    ///
    /// # Returns
    ///
    /// Line-by-line results with one-based line numbers.
    #[must_use]
    pub fn find_lines<'a>(&self, haystack: &'a str) -> Vec<LineMatch<'a>> {
        if matches!(self.engine, Engine::Approximate(_)) {
            if self.searches_lines_one_by_one() {
                return self.find_lines_sequential(haystack);
            }
            if parallel_lines_pay_off(haystack.len()) {
                return self.find_lines_chunk_parallel(haystack);
            }
            return self.find_lines_global(haystack);
        }

        if matches!(self.engine, Engine::Structured(_)) {
            return self.find_lines_global(haystack);
        }
        if matches!(self.engine, Engine::Boolean(_)) {
            // Queries hold on single lines, since no literal of theirs may hold a line feed,
            // so line-aligned chunks search apart. A match limit counts over the whole input.
            if self.config.max_matches().is_none() && parallel_lines_pay_off(haystack.len()) {
                return self.find_lines_chunk_parallel(haystack);
            }
            return self.find_lines_global(haystack);
        }

        if profiles::use_lines_fast_path(
            self.config,
            self.has_newline_patterns,
            haystack.len(),
            self.pattern_count(),
        ) {
            if parallel_lines_pay_off(haystack.len()) {
                return self.find_lines_chunk_parallel(haystack);
            }
            return self.find_lines_global(haystack);
        }

        self.find_lines_sequential(haystack)
    }

    /// Finds matches on each line in parallel using Rayon.
    ///
    /// # Arguments
    ///
    /// * `haystack` - Input text split by `str::lines`.
    ///
    /// # Returns
    ///
    /// Ordered line results equivalent to [`Searcher::find_lines`].
    #[must_use]
    pub fn find_lines_parallel<'a>(&self, haystack: &'a str) -> Vec<LineMatch<'a>> {
        if matches!(
            self.engine,
            Engine::Approximate(_) | Engine::Structured(_) | Engine::Boolean(_)
        ) || !profiles::use_lines_fast_path(
            self.config,
            self.has_newline_patterns,
            haystack.len(),
            self.pattern_count(),
        ) || !parallel_lines_pay_off(haystack.len())
        {
            return self.find_lines(haystack);
        }

        self.find_lines_chunk_parallel(haystack)
    }

    /// Searches newline-aligned chunks on the thread pool, one scan per chunk.
    fn find_lines_chunk_parallel<'a>(&self, haystack: &'a str) -> Vec<LineMatch<'a>> {
        let bytes = haystack.as_bytes();
        // Approximate engines parallelize within chunks, but line collection is serial per
        // chunk, so use several chunks per thread.
        let num_chunks = if matches!(self.engine, Engine::Approximate(_)) {
            (4 * rayon::current_num_threads()).min(bytes.len() / profiles::MIN_APPROX_LINE_CHUNK)
        } else {
            self.exact_chunk_count(bytes.len())
        };
        let splits = line_split_points(bytes, num_chunks);
        let planned = self.block_plan(bytes);

        let chunk_count = splits.len() - 1;
        let mut results = Vec::new();
        // Lines before the chunk being joined, which numbers its lines from one.
        let mut lines_before = 0_usize;
        merge_chunks(
            chunk_count,
            |index| {
                let chunk = &haystack[splits[index]..splits[index + 1]];
                self.collect_lines(chunk, planned, LineScope::Chunk)
                    .finish_with_line_count()
            },
            |index, (lines, line_count)| {
                if index == 0 {
                    results = adopt_first_chunk(lines, chunk_count, None);
                } else {
                    // Renumbered as they are copied, so each line is read once and its
                    // chunk's buffer, last written by another core, is never written here.
                    results.extend(lines.into_iter().map(|mut line| {
                        line.line_number += lines_before;
                        line
                    }));
                }
                lines_before += line_count;
            },
        );
        release_unused_capacity(&mut results);
        results
    }

    fn find_lines_sequential<'a>(&self, haystack: &'a str) -> Vec<LineMatch<'a>> {
        let limit = effective_limit(self.config.max_matches, None);
        haystack
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                let line_matches = self.find_sequential(line.as_bytes(), limit);
                if line_matches.is_empty() {
                    None
                } else {
                    Some(LineMatch {
                        line_number: index + 1,
                        line,
                        matches: SmallVec::from_vec(line_matches),
                    })
                }
            })
            .collect()
    }

    fn find_lines_global<'a>(&self, haystack: &'a str) -> Vec<LineMatch<'a>> {
        let planned = self.block_plan(haystack.as_bytes());
        self.collect_lines(haystack, planned, LineScope::Input(haystack))
            .finish_lines()
    }

    /// Collects the matching lines of `text` using the block plan of its whole input.
    fn collect_lines<'a>(
        &self,
        text: &'a str,
        planned: BlockPlan<'_>,
        scope: LineScope,
    ) -> LineMatchCollector<'a> {
        let expected_lines = planned
            .as_ref()
            .map_or(0, |(_, plan)| plan.expected_lines(text.len()));
        let mut collector = LineMatchCollector::new(text, expected_lines);
        self.visit_line_candidates(text.as_bytes(), planned, scope, |matched| {
            collector.record(matched);
        });
        collector
    }

    /// Streams the matches of `bytes` to `visit` in start order.
    ///
    /// Approximate engines report only line-local windows, so callers must check that they
    /// support line-local search. `planned` is the block plan for the whole input.
    fn visit_line_candidates(
        &self,
        bytes: &[u8],
        planned: BlockPlan<'_>,
        scope: LineScope,
        mut visit: impl FnMut(Match),
    ) {
        let is_chunk = scope == LineScope::Chunk;
        match &self.engine {
            Engine::Multi(engine) if self.streams_multi_matches() => {
                engine.visit_planned(bytes, planned, |matched| {
                    visit(matched);
                    true
                });
            }
            Engine::Approximate(engine) => engine
                .search_line_local(bytes, self.config)
                .into_iter()
                .for_each(visit),
            // Stream matches unless the whole input would first split into parallel chunks.
            Engine::Single(engine) if is_chunk || !self.should_parallelize(bytes.len()) => {
                engine.visit_planned(bytes, self.config, planned, |matched| {
                    visit(matched);
                    true
                });
            }
            // A chunk already runs on its own thread, so it must not split again.
            Engine::Single(_) | Engine::Multi(_) | Engine::Boolean(_) | Engine::Structured(_) => {
                let matches = match scope {
                    LineScope::Input(text) => self.find_with_limit(text, None),
                    LineScope::Chunk => self.find_sequential(bytes, usize::MAX),
                };
                matches.into_iter().for_each(visit);
            }
        }
    }

    /// Whether the multi-pattern engine streams its matches in start order, unchanged by
    /// overlap, word, or count rules that need them all first.
    fn streams_multi_matches(&self) -> bool {
        let config = self.config;
        !config.overlapping() && !config.whole_word() && config.max_matches().is_none()
    }

    /// Whether [`Self::visit_matching_lines`] finds in raw bytes the lines and matches that
    /// [`Self::find_lines`] finds in their text with invalid UTF-8 replaced. Replacing changes
    /// only the bytes of invalid sequences: a sequence's lead byte is never taken into an invalid
    /// one before it, and valid sequences decode as they are. So an exact UTF-8 pattern matches
    /// the same bytes either way, unless it holds the replacement character, which matches
    /// replaced bytes only in the text. Word bytes and case folding are ASCII.
    #[cfg(feature = "cli")]
    pub(crate) fn matches_bytes_as_text(&self) -> bool {
        !self.searches_lines_one_by_one()
            && !self
                .pattern_labels
                .iter()
                .any(|pattern| pattern.contains(char::REPLACEMENT_CHARACTER))
            && match &self.engine {
                Engine::Single(_) => true,
                // Unless they overlap, multi-pattern matches come in start order.
                Engine::Multi(_) => !self.config.overlapping(),
                Engine::Approximate(_) | Engine::Boolean(_) | Engine::Structured(_) => false,
            }
    }

    /// Streams the lines of `bytes` that hold matches to `visit` in order, on this thread,
    /// with their indexes if `COUNT`. `bytes` need not be UTF-8, and lines keep a carriage
    /// return before their line feed. Needs [`Self::matches_bytes_as_text`].
    ///
    /// Unless `how.with_matches`, lines come without their matches, found as matching lines
    /// are counted, which is faster. The search then also counts the line feeds it passes,
    /// unless `how.feeds` gives them, and looks for NUL bytes if `how.nuls`, in its one pass
    /// over the bytes where it can.
    ///
    /// # Returns
    ///
    /// The lines of `bytes` when `COUNT`, and zero otherwise. `None` if `how.nuls` and
    /// `bytes` hold a NUL byte: `visit` may then have been passed any of the lines.
    #[cfg(feature = "cli")]
    pub(crate) fn visit_matching_lines<const COUNT: bool>(
        &self,
        bytes: &[u8],
        how: LineVisit,
        mut visit: impl FnMut(MatchedLine),
    ) -> Option<usize> {
        let mut lines = MatchedLines::<COUNT>::new(bytes);
        if let Some(feeds) = how.feeds {
            lines.lines.note(Watched {
                len: bytes.len(),
                feeds,
                nul: false,
            });
        }
        let watch = Watch {
            nuls: how.nuls,
            feeds: COUNT && how.feeds.is_none(),
        };
        if how.with_matches {
            // The engine's search finds no NUL bytes, and the pass that does counts line
            // feeds for little more.
            if watch.nuls {
                let mut watched = Watched::default();
                watched.pass(bytes, watch);
                if watched.nul {
                    return None;
                }
                if watch.feeds {
                    lines.lines.note(watched);
                }
            }
            let planned = self.block_plan(bytes);
            self.visit_line_candidates(bytes, planned, LineScope::Chunk, |found| {
                lines.record(found, &mut visit);
            });
        } else {
            let counter = PlannedLineCounter::new(self, bytes, Extent::Piece { preceded_by: 0 });
            let watcher = Watcher::new(watch);
            let (_, watched) = counter.visit(bytes, &watcher, |at| {
                if watch.feeds {
                    lines.lines.note(watcher.watched());
                }
                lines.record_line(at, &mut visit);
            });
            if watched.nul {
                return None;
            }
            if watch.feeds {
                lines.lines.note(watched);
            }
        }
        Some(lines.finish(&mut visit))
    }

    fn count_matching_lines_parallel(&self, haystack: &[u8]) -> usize {
        let num_chunks = self.exact_chunk_count(haystack.len());
        let splits = line_split_points(haystack, num_chunks);
        // No more threads than chunks.
        let threads = NonZeroUsize::new(rayon::current_num_threads().min(splits.len() - 1))
            .expect("a pool has a thread, and a haystack split in two or more has a chunk");
        let counter = PlannedLineCounter::new(self, haystack, Extent::Whole { threads });
        map_chunks(splits.len() - 1, |index| {
            counter.count(&haystack[splits[index]..splits[index + 1]])
        })
        .into_iter()
        .sum()
    }

    /// Counts matching lines by searching each line separately. `haystack` need not be UTF-8,
    /// but the engine must not be structured.
    pub(super) fn count_lines_one_by_one(&self, haystack: &[u8]) -> usize {
        use bstr::ByteSlice;

        if self.config.max_matches() == Some(0) {
            return 0;
        }
        haystack
            .lines()
            .filter(|line| self.contains_in_bytes(line))
            .count()
    }

    /// Whether line searches must run line by line: a match limit applies per line, newline
    /// patterns must not match across lines, and some approximate engines lack a line-local
    /// search.
    pub(super) fn searches_lines_one_by_one(&self) -> bool {
        self.config.max_matches().is_some()
            || match &self.engine {
                Engine::Approximate(engine) => {
                    self.has_newline_patterns || !engine.supports_line_local_search()
                }
                Engine::Boolean(_) => false,
                Engine::Single(_) | Engine::Multi(_) | Engine::Structured(_) => {
                    self.has_newline_patterns
                }
            }
    }
}

/// An engine's plan for counting the lines of one input.
enum EnginePlan<'s> {
    Multi(&'s MultiPatternEngine, LineCountPlan<'s>),
    Boolean(&'s BooleanQueryEngine, BooleanLinePlan),
    Approximate(&'s ApproximateEngine, ApproximateLinePlan),
    /// The engine counts by its own search.
    Unplanned,
}

/// Counts matching lines in line-aligned pieces of one input.
///
/// Plans are made once from a sample of the input and shared by every piece. When the pair
/// scanner has a plan, the multi-pattern engine needs none.
pub(super) struct PlannedLineCounter<'s> {
    searcher: &'s Searcher,
    pairs: Option<(&'s PairScanner, PairPlan)>,
    engine: EnginePlan<'s>,
    /// Whether the pair scanner weighed its scans against the engine's search when planning.
    weighed: bool,
}

impl<'s> PlannedLineCounter<'s> {
    /// Plans counting from `sample`, the input or a piece of it described by `extent`.
    pub(super) fn new(searcher: &'s Searcher, sample: &[u8], extent: Extent) -> Self {
        let pairs = Self::plan_pairs(searcher, sample, extent);
        let config = searcher.config;
        let engine = match &searcher.engine {
            // The engine only counts the pair scanner's leftovers, which it plans for itself.
            Engine::Multi(_) if pairs.is_some() => EnginePlan::Unplanned,
            Engine::Multi(engine) => {
                EnginePlan::Multi(engine, engine.line_count_plan(sample, config))
            }
            Engine::Boolean(engine) => engine
                .line_count_plan(sample, config)
                .map_or(EnginePlan::Unplanned, |plan| {
                    EnginePlan::Boolean(engine, plan)
                }),
            Engine::Approximate(engine) => engine
                .line_count_plan(sample, config)
                .map_or(EnginePlan::Unplanned, |plan| {
                    EnginePlan::Approximate(engine, plan)
                }),
            Engine::Single(_) | Engine::Structured(_) => EnginePlan::Unplanned,
        };
        Self {
            searcher,
            pairs,
            engine,
            weighed: extent.weighs_scans(sample.len()),
        }
    }

    /// Returns the pair scanner with its plan, if the searcher has one that suits the input.
    fn plan_pairs(
        searcher: &'s Searcher,
        sample: &[u8],
        extent: Extent,
    ) -> Option<(&'s PairScanner, PairPlan)> {
        let scanner = searcher.line_pairs.as_ref()?;
        Some((scanner, scanner.plan(sample, extent)?))
    }

    /// Replans the pair scan from `piece` once the input proves long enough to weigh scans
    /// against the engine's search.
    ///
    /// Runs at most once, at the first such piece, and only if no pair plan exists yet. The
    /// engine keeps its plan.
    pub(super) fn weigh_scans(&mut self, piece: &[u8], extent: Extent) {
        if self.weighed || self.pairs.is_some() || !extent.weighs_scans(piece.len()) {
            return;
        }
        self.weighed = true;
        self.pairs = Self::plan_pairs(self.searcher, piece, extent);
    }

    /// Counts the matching lines of a line-aligned `piece`. The pair scanner, if planned,
    /// counts as far as it can and the engine counts the rest.
    pub(super) fn count(&self, piece: &[u8]) -> usize {
        self.visit(piece, &Watcher::default(), |_| {}).0
    }

    /// Counts like [`Self::count`], passing `on_line` an offset in each matching line, in
    /// order. Only the exact engines report lines: see [`Searcher::matches_bytes_as_text`].
    ///
    /// The piece is also watched for what `watcher` says as it is searched: by the pair
    /// scanner as far as it scans, as [`PairScanner::visit_lines`] tells, and, when NUL
    /// bytes are looked for, in a pass of its own where the engine searches. When `on_line`
    /// is passed a line, `watcher` holds what had been watched by then.
    ///
    /// # Returns
    ///
    /// The count, with what was watched of the piece. When NUL bytes are looked for, that
    /// tells whether the piece holds one, and if it does the count means nothing.
    pub(super) fn visit(
        &self,
        piece: &[u8],
        watcher: &Watcher,
        mut on_line: impl FnMut(usize),
    ) -> (usize, Watched) {
        let watch = watcher.watch();
        let (mut count, unscanned, mut watched) = match &self.pairs {
            Some((scanner, plan)) => {
                let scanned = scanner.visit_lines(plan, piece, watcher, &mut on_line);
                (scanned.count, scanned.unscanned, scanned.watched)
            }
            None => (0, 0, Watched::default()),
        };
        // The engine's search finds no NUL bytes, and the pass that does counts line feeds
        // for little more.
        if watch.nuls && !watched.nul {
            watched.pass(&piece[watched.len..], watch);
        }
        if !watched.nul {
            watcher.note(watched);
            count += self.visit_by_engine(&piece[unscanned..], |at| on_line(unscanned + at));
        }
        (count, watched)
    }

    fn visit_by_engine(&self, piece: &[u8], on_line: impl FnMut(usize)) -> usize {
        let searcher = self.searcher;
        match &self.engine {
            EnginePlan::Multi(engine, plan) => engine.visit_matching_lines_planned(
                plan,
                piece,
                (searcher.config, &searcher.pattern_lengths),
                on_line,
            ),
            EnginePlan::Boolean(engine, plan) => {
                engine.count_planned_lines(piece, searcher.config, plan)
            }
            EnginePlan::Approximate(engine, plan) => {
                engine.count_planned_lines(piece, searcher.config, plan)
            }
            EnginePlan::Unplanned => searcher.visit_matching_lines_by_engine(piece, on_line),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Extent, LineMatchCollector, Match, PlannedLineCounter};
    use crate::{CaseMode, SearchConfig, Searcher};

    /// A piece far enough into its input that scans are always weighed.
    const LATER_PIECE: Extent = Extent::Piece {
        preceded_by: usize::MAX,
    };

    #[test]
    fn pair_scanned_line_counts_match_per_line_searches() {
        let body = (0..16_000)
            .map(|index| {
                let level = ["INFO", "warn", "ERROR", "Error", "DEBUG"][index % 5];
                format!(
                    "{index:05} {level} request_id={index} status={} {}{}{}\n",
                    200 + index % 9,
                    "x".repeat(index % 31),
                    if index % 11 == 0 {
                        " caf\u{e9} \u{20ac}"
                    } else {
                        ""
                    },
                    if index % 3 == 0 { "\r" } else { "" }
                )
            })
            .collect::<String>();
        let insensitive = SearchConfig::builder()
            .case_mode(CaseMode::AsciiInsensitive)
            .build();
        let overlapping = SearchConfig::builder().overlapping(true).build();
        let sensitive = SearchConfig::default();
        // The flag says whether the patterns must get a pair scan.
        let cases: [(&[&str], SearchConfig, bool); 9] = [
            (&["ERROR"], sensitive, true),
            (&["request_id=1999"], sensitive, true),
            (&["ERROR", "warn", "status=204"], sensitive, true),
            (&["ERROR", "status=204"], overlapping, true),
            (&["error", "status=204", "ID=59"], insensitive, true),
            (&["REQUEST_ID=15999 STATUS"], insensitive, true),
            (&["\u{e9} \u{20ac}", "ID=77"], insensitive, true),
            // Only some targets pair-scan a set this large.
            (
                &[
                    "ERROR",
                    "status=204",
                    "request_id=1999",
                    "ID=59",
                    "ID=77",
                    "caf\u{e9}",
                ],
                sensitive,
                cfg!(target_arch = "aarch64"),
            ),
            // Every line holds a match.
            (&["request_id=", "status=20", "xx"], sensitive, false),
        ];
        // The unterminated last line ends in three-byte characters, so for some `shift` the
        // pair scanner's leftover bytes start mid-character.
        let mut split_characters = 0;
        for shift in 0..3 {
            let haystack = format!(
                "{body}tail ERROR status=204 {}{}",
                "x".repeat(shift),
                "\u{20ac}".repeat(40)
            );
            let bytes = haystack.as_bytes();
            for (patterns, config, pair_scanned) in cases {
                let searcher = Searcher::new(patterns, config).expect("literal patterns compile");
                let counter = PlannedLineCounter::new(&searcher, bytes, LATER_PIECE);
                assert!(counter.pairs.is_some() || !pair_scanned, "{patterns:?}");
                if let Some((scanner, plan)) = &counter.pairs {
                    let (_, unscanned) = scanner.count_lines(plan, bytes);
                    split_characters += usize::from(!haystack.is_char_boundary(unscanned));
                }

                let whole = searcher.count_matching_lines(&haystack);
                let planned = counter.count(bytes);

                // An oracle that shares no code with the engines.
                let fold = |text: &str| {
                    if config.case_mode() == CaseMode::AsciiInsensitive {
                        text.to_ascii_lowercase()
                    } else {
                        text.to_owned()
                    }
                };
                let folded_patterns: Vec<String> =
                    patterns.iter().map(|pattern| fold(pattern)).collect();
                let expected = haystack
                    .lines()
                    .filter(|line| {
                        let line = fold(line);
                        folded_patterns
                            .iter()
                            .any(|pattern| line.contains(pattern.as_str()))
                    })
                    .count();
                assert!(expected > 0);
                assert_eq!(whole, expected, "{patterns:?} {shift}");
                assert_eq!(planned, expected, "{patterns:?} {shift}");
            }
        }
        assert!(split_characters > 0);
    }

    #[test]
    fn a_counter_without_a_pair_scan_weighs_one_once_its_input_has_proved_long() {
        // The patterns' rarest pairs collide, so only a weighed scan of other pairs serves
        // them, while a single pattern needs no weighing. The matches lie outside the sample.
        let dots = "................................................\n";
        let text = [&dots.repeat(100), "abcd\naefx\ngbcd\n", &dots.repeat(2_900)].concat();
        let piece = text.as_bytes();
        let first_piece = Extent::Piece { preceded_by: 0 };
        let colliding = Searcher::new(["abcd", "aefx", "gbcd"], SearchConfig::default())
            .expect("literal patterns compile");
        let single =
            Searcher::new(["abcd"], SearchConfig::default()).expect("literal patterns compile");
        let mut short = PlannedLineCounter::new(&colliding, piece, first_piece);
        let mut long = PlannedLineCounter::new(&colliding, piece, first_piece);
        let mut planned = PlannedLineCounter::new(&single, piece, first_piece);

        let scanned_at_first = [&short, &long, &planned].map(|counter| counter.pairs.is_some());
        let rarest_pair = planned.pairs.clone();
        short.weigh_scans(piece, first_piece);
        long.weigh_scans(piece, LATER_PIECE);
        planned.weigh_scans(piece, LATER_PIECE);
        let counts = [&short, &long, &planned].map(|counter| counter.count(piece));

        assert_eq!(scanned_at_first, [false, false, true]);
        assert!(short.pairs.is_none() && !short.weighed);
        // Scans are weighed on aarch64 alone.
        assert_eq!(long.pairs.is_some(), cfg!(target_arch = "aarch64"));
        assert_eq!(long.weighed, cfg!(target_arch = "aarch64"));
        assert_eq!(planned.pairs, rarest_pair);
        assert_eq!(counts, [3, 3, 1]);
    }

    #[test]
    fn searches_a_pair_scan_cannot_serve_count_without_it() {
        // `ERROR` is a whole word on every fourth line only.
        let haystack = (0..4_000)
            .map(|index| {
                let word = if index % 4 == 0 { "ERROR" } else { "XERRORX" };
                format!("{index:05} {word} padding padding padding padding\r\n")
            })
            .collect::<String>();
        let whole_word = SearchConfig::builder().whole_word(true).build();
        let sensitive = SearchConfig::default();
        // A pair scan checks words around its candidates.
        for patterns in [&["ERROR"][..], &["ERROR", "absent"]] {
            let searcher = Searcher::new(patterns, whole_word).expect("literal patterns compile");
            assert!(searcher.line_pairs.is_some(), "{patterns:?}");
            assert_eq!(
                searcher.count_matching_lines(&haystack),
                1_000,
                "{patterns:?}"
            );
        }
        let cases: [(&[&str], SearchConfig, usize); 3] = [
            // More patterns than a pair scan takes on any target.
            (
                &["ERROR", "a1", "b2", "c3", "d4", "e5", "f6", "g7", "h8"],
                sensitive,
                4_000,
            ),
            (&["X"], sensitive, 3_000),
            (&["X", "ERROR"], sensitive, 4_000),
        ];
        // No line matches: `str::lines` strips the `\r`.
        let line_end = Searcher::new(["padding\r"], sensitive).expect("the pattern compiles");
        assert!(line_end.line_pairs.is_none());
        assert_eq!(
            line_end.count_matching_lines(&haystack),
            haystack
                .lines()
                .filter(|line| line.contains("padding\r"))
                .count()
        );
        for (patterns, config, expected) in cases {
            let searcher = Searcher::new(patterns, config).expect("literal patterns compile");
            assert!(searcher.line_pairs.is_none(), "{patterns:?}");

            let count = searcher.count_matching_lines(&haystack);

            assert_eq!(count, expected, "{patterns:?}");
        }
    }

    #[test]
    fn parallel_boolean_line_count_matches_per_line_evaluation() {
        let haystack = (0..20_000)
            .map(|index| {
                let level = ["ERROR", "WARN", "INFO"][index % 3];
                let status = if index % 7 == 0 { 500 } else { 200 };
                format!(
                    "{index} {level} request status={status} {}\n",
                    "x".repeat(index % 40)
                )
            })
            .collect::<String>();
        let config = crate::SearchConfig::builder().boolean_queries(true).build();
        let searcher = crate::Searcher::new(["ERROR AND status=500 OR (WARN AND NOT x)"], config)
            .expect("boolean query should compile");
        assert!(super::parallel_lines_pay_off(haystack.len()));

        let count = searcher.count_matching_lines(&haystack);

        let expected = haystack
            .lines()
            .filter(|line| searcher.contains(line))
            .count();
        assert!(expected > 0);
        assert_eq!(count, expected);
    }

    #[test]
    fn boolean_lines_found_in_parallel_chunks_are_the_lines_one_search_finds() {
        let mut haystack = (0..60_000)
            .map(|index| {
                let level = ["ERROR", "WARN", "ok"][index % 3];
                let retry = if index % 4 == 0 { " retry" } else { "" };
                let ending = if index % 5 == 0 { "\r\n" } else { "\n" };
                let blank = if index % 97 == 0 { "\n" } else { "" };
                format!("{index} {level}{retry} caf\u{e9} yx{ending}{blank}")
            })
            .collect::<String>();
        haystack.push_str("ERROR without a line feed");
        let config = crate::SearchConfig::builder().boolean_queries(true).build();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("a small pool starts");
        assert!(pool.install(|| super::parallel_lines_pay_off(haystack.len())));

        for query in [
            "NOT ok",
            "ERROR AND NOT retry",
            "(WARN OR ERROR) AND retry",
            "NOT (yx OR caf\u{e9})",
        ] {
            let searcher =
                crate::Searcher::new([query], config).expect("boolean query should compile");

            let (found, in_chunks, whole) = pool.install(|| {
                (
                    searcher.find_lines(&haystack),
                    searcher.find_lines_chunk_parallel(&haystack),
                    searcher.find_lines_global(&haystack),
                )
            });

            assert!(!whole.is_empty(), "{query}");
            assert_eq!(found, whole, "{query}");
            assert_eq!(in_chunks, whole, "{query}");
        }
    }

    #[test]
    fn boolean_literals_cannot_span_the_lines_chunks_are_cut_at() {
        let config = crate::SearchConfig::builder().boolean_queries(true).build();

        let over_a_line_feed = crate::Searcher::new(["NOT \"x\ny\""], config);
        let over_a_carriage_return = crate::Searcher::new(["\"x\ry\" OR z"], config);

        assert!(over_a_line_feed.is_err());
        assert!(over_a_carriage_return.is_err());
    }

    #[test]
    fn matches_reaching_a_stripped_carriage_return_stay_inside_the_line() {
        let haystack = "ab\r\nab\r\n";
        let mut collector = LineMatchCollector::new(haystack, 0);
        // "b\r" on the first line, then "b" and a lone "\r" on the second.
        for (start, end) in [(1, 3), (5, 6), (6, 7)] {
            collector.record(Match {
                start,
                end,
                pattern_index: 0,
            });
        }

        let lines = collector.finish_lines();

        let spans = lines
            .iter()
            .map(|line| {
                let texts = line
                    .matches
                    .iter()
                    .map(|matched| &line.line[matched.start..matched.end])
                    .collect::<Vec<_>>();
                (line.line, texts)
            })
            .collect::<Vec<_>>();
        assert_eq!(spans, [("ab", vec!["b"]), ("ab", vec!["b", ""])]);
    }

    #[test]
    fn line_collector_numbers_sparse_matches_like_str_lines() {
        let filler = format!("{}\n", "c".repeat(200)).repeat(5);
        let haystack = format!("a\n\nxx b\r\n{filler}\n\nyy b\n{}z b", "\n".repeat(600));
        let haystack = haystack.as_str();
        let mut collector = LineMatchCollector::new(haystack, 0);
        for start in haystack.match_indices('b').map(|(start, _)| start) {
            collector.record(Match {
                start,
                end: start + 1,
                pattern_index: 0,
            });
        }

        let (lines, line_count) = collector.finish_with_line_count();

        let expected = haystack
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains('b'))
            .map(|(index, line)| (index + 1, line))
            .collect::<Vec<_>>();
        let actual = lines
            .iter()
            .map(|line| (line.line_number, line.line))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_eq!(line_count, haystack.lines().count());
    }

    #[test]
    fn line_collector_skips_a_match_starting_on_the_newline_boundary() {
        let mut collector = LineMatchCollector::new("a\nb", 0);
        collector.record(Match {
            start: 1,
            end: 2,
            pattern_index: 0,
        });
        assert!(collector.finish_lines().is_empty());
    }
}

#[cfg(all(test, feature = "cli"))]
mod streamed_tests {
    use super::{
        LINE_JUMP_MIN_GAP, LineVisit, Match, MatchedLine, MatchedLines, PlannedLineCounter,
    };
    use crate::exact::Extent;
    use crate::{CaseMode, SearchConfig, Searcher};

    /// A visit that counts the line feeds it passes itself.
    fn how(with_matches: bool, nuls: bool) -> LineVisit {
        LineVisit {
            with_matches,
            nuls,
            feeds: None,
        }
    }

    /// The lines of `bytes` holding the `found` matches, as their text, index, and match
    /// offsets, and the line count.
    type Grouped = (Vec<(Vec<u8>, usize, Vec<(usize, usize)>)>, usize);

    fn grouped<const COUNT: bool>(bytes: &[u8], found: &[(usize, usize)]) -> Grouped {
        let mut lines = MatchedLines::<COUNT>::new(bytes);
        let mut visited = Vec::new();
        let mut visit = |line: MatchedLine| {
            let spans = line.matches.iter().map(|found| (found.start, found.end));
            visited.push((
                bytes[line.start..line.end].to_vec(),
                line.index,
                spans.collect(),
            ));
        };
        for &(start, end) in found {
            let found = Match {
                start,
                end,
                pattern_index: 0,
            };
            lines.record(found, &mut visit);
        }
        let line_count = lines.finish(&mut visit);
        (visited, line_count)
    }

    #[test]
    fn matched_lines_keep_raw_bytes_and_count_lines_only_when_asked() {
        let far = LINE_JUMP_MIN_GAP;
        let bytes = [
            &b"hit hit\r\n\xff hit\n"[..],
            &b"x\n".repeat(far),
            b"last hit",
        ]
        .concat();
        let last = bytes.len() - 3;
        let found = [(0, 3), (4, 7), (11, 14), (last, last + 3)];

        let (lines, line_count) = grouped::<true>(&bytes, &found);
        assert_eq!(
            lines,
            [
                (b"hit hit\r".to_vec(), 0, vec![(0, 3), (4, 7)]),
                (b"\xff hit".to_vec(), 1, vec![(2, 5)]),
                (b"last hit".to_vec(), far + 2, vec![(5, 8)]),
            ]
        );
        assert_eq!(line_count, far + 3);
        let (uncounted, line_count) = grouped::<false>(&bytes, &found);
        assert!(uncounted.iter().all(|(_, index, _)| *index == 0));
        assert_eq!(line_count, 0);
    }

    #[test]
    fn matching_lines_stream_from_the_engines_like_found_lines() {
        let searcher =
            Searcher::new(["hit"], SearchConfig::default()).expect("searcher should compile");
        let text = format!(
            "hit hit\r\nmiss\n{}caf\u{e9} hit\nlast hit",
            "x\n".repeat(300)
        );
        let mut visited = Vec::new();

        let line_count =
            searcher.visit_matching_lines::<true>(text.as_bytes(), how(true, false), |line| {
                visited.push((line.index + 1, line.matches.len()));
            });

        let found = searcher.find_lines(&text);
        let expected = found
            .iter()
            .map(|line| (line.line_number, line.matches.len()));
        assert!(searcher.matches_bytes_as_text());
        assert_eq!(visited, expected.collect::<Vec<_>>());
        assert_eq!(line_count, Some(text.lines().count()));
    }

    #[test]
    fn lines_found_as_counted_are_the_lines_found_with_their_matches() {
        // Long enough for pair scans and anchored searches, with non-ASCII patterns beside
        // invalid UTF-8, sparse and dense.
        let kinds: [&[u8]; 7] = [
            b"plain words here\n",
            "\u{436}\u{451}\u{43b}\u{442}\u{44b}\u{439} caf\u{e9} \u{43a}\u{440}\u{443}\u{433}\n"
                .as_bytes(),
            b"\xff\xfe caf\xc3\xa9 hit\r\n",
            b"status=503 ok\n",
            "\u{6771}\u{4eac}\u{30bf}\u{30ef}\u{30fc} view\n".as_bytes(),
            b"\xc3caf\xc3\xa9\xa9 tail\n",
            b"\n",
        ];
        let mut text = Vec::new();
        for index in 0..60_000_usize {
            text.extend_from_slice(kinds[(index * 7 + index / 13) % kinds.len()]);
        }
        // The scans stop short of the end, so the last line matches every set, for the
        // engine to find.
        let last = "Status=503 STATUS=503 caf\u{e9} hit \u{436}\u{451}\u{43b}\u{442}\u{44b}\u{439} \
                    \u{6771}\u{4eac}\u{30bf}\u{30ef}\u{30fc}";
        text.extend_from_slice(last.as_bytes());
        let pattern_sets: [&[&str]; 5] = [
            &["caf\u{e9}"],
            &["\u{436}\u{451}\u{43b}\u{442}\u{44b}\u{439}"],
            &[
                "caf\u{e9}",
                "STATUS=503",
                "\u{6771}\u{4eac}\u{30bf}\u{30ef}\u{30fc}",
            ],
            &["hit", "plain", "tail", "ok", "words"],
            &["Status=50", "view", "\u{442}\u{44b}"],
        ];
        let mut paired = [false; 2];
        for patterns in pattern_sets {
            for (case_mode, whole_word) in [
                (CaseMode::Sensitive, false),
                (CaseMode::AsciiInsensitive, false),
                (CaseMode::Sensitive, true),
                (CaseMode::AsciiInsensitive, true),
            ] {
                let config = SearchConfig::builder()
                    .case_mode(case_mode)
                    .whole_word(whole_word)
                    .build();
                let searcher = Searcher::new(patterns, config).expect("searcher should compile");
                assert!(searcher.matches_bytes_as_text());
                let extent = Extent::Piece { preceded_by: 0 };
                let planned = PlannedLineCounter::new(&searcher, &text, extent)
                    .pairs
                    .is_some();
                paired[usize::from(planned)] = true;
                let lines_of = |with_matches| {
                    let mut lines = Vec::new();
                    let count = searcher.visit_matching_lines::<true>(
                        &text,
                        how(with_matches, false),
                        |line| {
                            assert_eq!(line.matches.is_empty(), !with_matches);
                            lines.push((line.start, line.end, line.index));
                        },
                    );
                    (lines, count)
                };
                let (counted, with_matches) = (lines_of(false), lines_of(true));
                assert_eq!(
                    counted, with_matches,
                    "{patterns:?} {case_mode:?} {whole_word}"
                );
                assert_eq!(
                    counted.1,
                    Some(text.split_inclusive(|&byte| byte == b'\n').count())
                );
                // Looking for NUL bytes in the same pass finds the same lines.
                for with_matches in [false, true] {
                    let mut lines = Vec::new();
                    let count = searcher.visit_matching_lines::<true>(
                        &text,
                        how(with_matches, true),
                        |line| {
                            lines.push((line.start, line.end, line.index));
                        },
                    );
                    assert_eq!((lines, count), counted, "{patterns:?} {with_matches}");
                    let mut unnumbered = Vec::new();
                    let count = searcher.visit_matching_lines::<false>(
                        &text,
                        how(with_matches, true),
                        |line| {
                            unnumbered.push((line.start, line.end, line.index));
                        },
                    );
                    let starts = |lines: &[(usize, usize, usize)]| {
                        lines
                            .iter()
                            .map(|line| (line.0, line.1))
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(starts(&unnumbered), starts(&counted.0));
                    assert!(unnumbered.iter().all(|line| line.2 == 0));
                    assert_eq!(count, Some(0));
                }
                let naive = text
                    .split(|&byte| byte == b'\n')
                    .filter(|line| {
                        patterns.iter().any(|pattern| {
                            let end = |start: usize| start + pattern.len();
                            line.windows(pattern.len())
                                .enumerate()
                                .any(|(start, window)| {
                                    let equal = match case_mode {
                                        CaseMode::Sensitive => window == pattern.as_bytes(),
                                        CaseMode::AsciiInsensitive => {
                                            window.eq_ignore_ascii_case(pattern.as_bytes())
                                        }
                                    };
                                    equal
                                        && (!whole_word
                                            || crate::is_word_boundary(line, start, end(start)))
                                })
                        })
                    })
                    .count();
                assert_eq!(
                    counted.0.len(),
                    naive,
                    "{patterns:?} {case_mode:?} {whole_word}"
                );
                assert_eq!(
                    counted.0.len(),
                    searcher.count_matching_lines_in_bytes(&text)
                );
            }
        }
        assert_eq!(paired, [true; 2], "both with and without a pair scan");
    }

    /// Text long enough for a pair scan, whose lines hold `hit` every `spacing` lines.
    fn spaced_hits(lines: usize, spacing: usize) -> Vec<u8> {
        let mut text = Vec::new();
        for index in 0..lines {
            let line = if index % spacing == 0 {
                format!("line {index} with a hit in it\n")
            } else {
                format!("line {index} of plain filler text\n")
            };
            text.extend_from_slice(line.as_bytes());
        }
        text
    }

    #[test]
    fn a_nul_byte_anywhere_ends_a_visit_that_looks_for_one() {
        let searcher =
            Searcher::new(["hit"], SearchConfig::default()).expect("searcher should compile");
        let text = spaced_hits(20_000, 97);
        let extent = Extent::Piece { preceded_by: 0 };
        assert!(
            PlannedLineCounter::new(&searcher, &text, extent)
                .pairs
                .is_some()
        );
        let short = &text[..2_000];
        assert!(
            PlannedLineCounter::new(&searcher, short, extent)
                .pairs
                .is_none()
        );

        for (text, at) in [
            (&text[..], 0),
            (&text[..], 1),
            (&text[..], 70_000),
            (&text[..], text.len() - 40),
            (&text[..], text.len() - 1),
            (short, 0),
            (short, 1_999),
        ] {
            let mut bytes = text.to_vec();
            bytes[at] = 0;
            for with_matches in [false, true] {
                let looked =
                    searcher.visit_matching_lines::<true>(&bytes, how(with_matches, true), |_| {});
                let unnumbered =
                    searcher.visit_matching_lines::<false>(&bytes, how(with_matches, true), |_| {});
                let searched =
                    searcher.visit_matching_lines::<true>(&bytes, how(with_matches, false), |_| {});

                assert_eq!((looked, unnumbered), (None, None), "NUL at {at}");
                assert_eq!(searched, Some(crate::count_lines(&bytes)));
            }
        }
    }

    #[test]
    fn lines_number_alike_whether_the_scan_or_the_cursor_counts_them() {
        let config = SearchConfig::default();
        // Sparse, dense, and every line; with and without a last line feed.
        for spacing in [1, 3, 97, 4_001, 50_000] {
            for cut in [0, 1, 12] {
                let text = spaced_hits(30_000, spacing);
                let text = &text[..text.len() - cut];
                for patterns in [&["hit"][..], &["hit", "line 29999 "]] {
                    let searcher =
                        Searcher::new(patterns, config).expect("searcher should compile");
                    let indexes = |with_matches, feeds| {
                        let mut indexes = Vec::new();
                        let how = LineVisit {
                            feeds,
                            ..how(with_matches, true)
                        };
                        let count = searcher.visit_matching_lines::<true>(text, how, |line| {
                            indexes.push(line.index);
                        });
                        (indexes, count)
                    };
                    let feeds = Some(text.iter().filter(|&&byte| byte == b'\n').count());

                    let (scanned, by_cursor) = (indexes(false, None), indexes(true, None));
                    let (counted_before, both) = (indexes(false, feeds), indexes(true, feeds));

                    let holds_one = |line: &&[u8]| {
                        patterns.iter().any(|pattern| {
                            line.windows(pattern.len())
                                .any(|window| window == pattern.as_bytes())
                        })
                    };
                    let expected: Vec<usize> = text
                        .split(|&byte| byte == b'\n')
                        .enumerate()
                        .filter(|(_, line)| holds_one(line))
                        .map(|(index, _)| index)
                        .collect();
                    assert_eq!(scanned.0, expected, "every {spacing} lines, cut {cut}");
                    assert_eq!(scanned, by_cursor);
                    assert_eq!(scanned, counted_before);
                    assert_eq!(scanned, both);
                    assert_eq!(scanned.1, Some(30_000));
                }
            }
        }
    }

    #[test]
    fn exact_patterns_without_the_replacement_character_match_bytes_as_text() {
        let config = SearchConfig::default();
        let overlapping = SearchConfig::builder().overlapping(true).build();
        let searcher = |patterns: &[&str], config| {
            Searcher::new(patterns, config).expect("searcher should compile")
        };

        assert!(searcher(&["hit", "miss"], config).matches_bytes_as_text());
        assert!(!searcher(&["hit", "miss"], overlapping).matches_bytes_as_text());
        assert!(searcher(&["caf\u{e9}", "\u{416}"], config).matches_bytes_as_text());
        assert!(!searcher(&["hit", "\u{fffd}"], config).matches_bytes_as_text());
    }
}
