use std::{borrow::Cow, cmp::Reverse};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use grep_matcher::{LineMatchKind, Matcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use memchr::{memchr, memrchr};
use smallvec::SmallVec;

use crate::chunks::fill_chunks;
use crate::{CaseMode, Match, SearchConfig, SearchError, profiles};

const WORD_TABLE: [bool; 256] = word_table();

const fn word_table() -> [bool; 256] {
    let mut table = [false; 256];
    let mut index = 0_usize;
    while index < 256 {
        let byte = index as u8;
        table[index] = byte.is_ascii_alphanumeric() || byte == b'_';
        index += 1;
    }
    table
}

pub(crate) fn build_automaton(
    patterns: &[Vec<u8>],
    config: SearchConfig,
    match_kind: MatchKind,
) -> Result<AhoCorasick, SearchError> {
    let max_pattern_len = patterns.iter().map(Vec::len).max().unwrap_or(1);
    let mut builder = AhoCorasickBuilder::new();
    builder.match_kind(match_kind);
    builder.ascii_case_insensitive(config.case_mode() == CaseMode::AsciiInsensitive);
    builder.kind(profiles::aho_kind(patterns.len(), max_pattern_len));
    builder
        .build(patterns)
        .map_err(|error| SearchError::AutomatonBuild {
            message: error.to_string(),
        })
}

pub(crate) fn build_line_matcher(
    patterns: &[Vec<u8>],
    config: SearchConfig,
) -> Result<RegexMatcher, SearchError> {
    let mut builder = RegexMatcherBuilder::new();
    builder.fixed_strings(true);
    builder.case_insensitive(config.case_mode() == CaseMode::AsciiInsensitive);
    builder.line_terminator(Some(b'\n'));
    builder.unicode(false);

    let pattern_refs = patterns
        .iter()
        .map(|pattern| {
            std::str::from_utf8(pattern).expect("patterns originate from UTF-8 search strings")
        })
        .collect::<Vec<_>>();

    builder
        .build_literals(pattern_refs.as_slice())
        .map_err(|error| SearchError::AutomatonBuild {
            message: error.to_string(),
        })
}

/// Appends the prioritized matches of a group sharing one end and clears it. Returns whether
/// `limit` was reached; callers must not call it again after that.
pub(crate) fn flush_multi_group(
    pending: &mut SmallVec<[Match; 4]>,
    haystack: &[u8],
    config: SearchConfig,
    pattern_lengths: &[usize],
    matches: &mut Vec<Match>,
    last_end: &mut usize,
    limit: usize,
) -> bool {
    pending.sort_unstable_by_key(|entry| {
        (
            Reverse(pattern_lengths[entry.pattern_index]),
            entry.pattern_index,
        )
    });

    let remaining = limit - matches.len();
    let group_limit = if config.overlapping() {
        remaining
    } else {
        remaining.min(1)
    };
    matches.extend(
        pending
            .iter()
            .copied()
            .filter(|candidate| {
                (!config.whole_word() || is_word_boundary(haystack, candidate.start, candidate.end))
                    && (config.overlapping() || candidate.start >= *last_end)
            })
            .take(group_limit),
    );
    if !config.overlapping()
        && let Some(last) = matches.last()
    {
        *last_end = last.end;
    }
    pending.clear();
    matches.len() == limit
}

pub(crate) fn select_best_pending_match(
    pending: &[Match],
    haystack: &[u8],
    config: SearchConfig,
    pattern_lengths: &[usize],
) -> Option<Match> {
    pending
        .iter()
        .copied()
        .filter(|candidate| {
            !config.whole_word() || is_word_boundary(haystack, candidate.start, candidate.end)
        })
        .min_by_key(|candidate| {
            (
                Reverse(pattern_lengths[candidate.pattern_index]),
                candidate.pattern_index,
            )
        })
}

#[inline]
pub(crate) fn window_matches_ascii_insensitive(window: &[u8], pattern: &[u8]) -> bool {
    window
        .iter()
        .copied()
        .zip(pattern.iter().copied())
        .all(|(left, right)| fold_ascii(left) == right)
}

/// UTF-8 byte order mark, written by some editors.
pub(crate) const BYTE_ORDER_MARK: &str = "\u{feff}";

#[inline]
pub(crate) fn normalize_haystack<'a>(haystack: &'a [u8], case_mode: CaseMode) -> Cow<'a, [u8]> {
    match case_mode {
        CaseMode::Sensitive => Cow::Borrowed(haystack),
        CaseMode::AsciiInsensitive => Cow::Owned(fold_ascii_bytes(haystack)),
    }
}

/// Returns a copy of `haystack` with ASCII uppercase lowered, folding in parallel from
/// [`profiles::PARALLEL_PASS_LEN`] bytes.
#[inline]
pub(crate) fn fold_ascii_bytes(haystack: &[u8]) -> Vec<u8> {
    if haystack.len() < profiles::PARALLEL_PASS_LEN || !profiles::has_parallel_workers() {
        // The branch-free fold has no table lookup, so this loop compiles to SIMD lanes.
        return haystack.iter().copied().map(fold_ascii).collect();
    }
    fold_ascii_bytes_parallel(haystack)
}

/// The parallel branch of [`fold_ascii_bytes`], kept out of line so the inlined serial
/// branch stays small.
#[inline(never)]
fn fold_ascii_bytes_parallel(haystack: &[u8]) -> Vec<u8> {
    // Large zeroed allocations come pre-zeroed from the OS, so this writes nothing.
    let mut folded = vec![0_u8; haystack.len()];
    fill_chunks(
        &mut folded,
        profiles::PARALLEL_PASS_CHUNK,
        |chunk, piece| {
            let source = &haystack[chunk * profiles::PARALLEL_PASS_CHUNK..];
            for (folded, &byte) in piece.iter_mut().zip(source) {
                *folded = fold_ascii(byte);
            }
        },
    );
    folded
}

/// Appends `bytes[start..end]` to `path`. Short spans copy a fixed 16-byte window and
/// truncate, avoiding a `memcpy` call.
#[inline(always)]
pub(crate) fn append_span(path: &mut Vec<u8>, bytes: &[u8], start: usize, end: usize) {
    const WINDOW: usize = 16;
    match bytes.get(start..).and_then(<[u8]>::first_chunk::<WINDOW>) {
        Some(window) if end - start <= WINDOW => {
            let path_len = path.len();
            path.extend_from_slice(window);
            path.truncate(path_len + end - start);
        }
        _ => path.extend_from_slice(&bytes[start..end]),
    }
}

/// Equality of equal-length short slices without `memcmp`, comparing two overlapping words.
#[inline(always)]
pub(crate) fn short_bytes_eq(left: &[u8], right: &[u8]) -> bool {
    debug_assert_eq!(left.len(), right.len());
    match left.len() {
        8..=16 => {
            left.first_chunk::<8>() == right.first_chunk::<8>()
                && left.last_chunk::<8>() == right.last_chunk::<8>()
        }
        4..8 => {
            left.first_chunk::<4>() == right.first_chunk::<4>()
                && left.last_chunk::<4>() == right.last_chunk::<4>()
        }
        _ => left == right,
    }
}

pub(crate) fn select_anchor(pattern: &[u8]) -> (usize, u8) {
    let mut frequencies = [0_u8; 256];
    for byte in pattern.iter().copied() {
        frequencies[usize::from(byte)] = frequencies[usize::from(byte)].saturating_add(1);
    }

    let mut best_index = 0_usize;
    let mut best_frequency = u8::MAX;
    for (index, byte) in pattern.iter().copied().enumerate() {
        let frequency = frequencies[usize::from(byte)];
        if frequency < best_frequency {
            best_frequency = frequency;
            best_index = index;
            if frequency == 1 {
                break;
            }
        }
    }

    (best_index, pattern[best_index])
}

#[inline]
pub(crate) fn normalize_pattern(pattern: &[u8], case_mode: CaseMode) -> Vec<u8> {
    normalize_haystack(pattern, case_mode).into_owned()
}

#[inline(always)]
pub(crate) fn fold_ascii(byte: u8) -> u8 {
    // Branch-free: one unsigned comparison selects 'A'..='Z', and bit 5 lowercases it.
    byte | (u8::from(byte.wrapping_sub(b'A') < 26) << 5)
}

#[inline]
pub(crate) fn unfold_ascii(byte: u8) -> u8 {
    if byte.is_ascii_lowercase() {
        byte - (b'a' - b'A')
    } else {
        byte
    }
}

#[inline]
pub(crate) fn effective_limit(max_matches: Option<usize>, limit_override: Option<usize>) -> usize {
    let default_limit = max_matches.unwrap_or(usize::MAX);
    match limit_override {
        Some(override_limit) => default_limit.min(override_limit),
        None => default_limit,
    }
}

#[inline]
pub(crate) fn is_word_boundary(haystack: &[u8], start: usize, end: usize) -> bool {
    let has_left_boundary = start == 0 || !is_word_byte(haystack[start - 1]);
    let has_right_boundary = end == haystack.len() || !is_word_byte(haystack[end]);
    has_left_boundary && has_right_boundary
}

#[inline]
fn is_word_byte(byte: u8) -> bool {
    WORD_TABLE[usize::from(byte)]
}

/// How many word bytes `bytes` starts with.
#[inline]
pub(crate) fn word_run_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|&byte| !is_word_byte(byte))
        .unwrap_or(bytes.len())
}

/// Returns the offset of the line feed ending the line at `position`, or `haystack.len()`.
#[inline]
pub(crate) fn line_end_from(haystack: &[u8], position: usize) -> usize {
    memchr(b'\n', &haystack[position..]).map_or(haystack.len(), |offset| position + offset)
}

/// Returns the exclusive end of a line's content, excluding a trailing carriage return.
#[inline]
pub(crate) fn line_content_end(haystack: &[u8], line_start: usize, line_end: usize) -> usize {
    line_end - usize::from(line_end > line_start && haystack[line_end - 1] == b'\r')
}

/// Like [`line_content_end`], but strips a carriage return only before a line feed, as
/// `str::lines` does.
#[inline]
pub(crate) fn terminated_line_content_end(
    haystack: &[u8],
    line_start: usize,
    line_end: usize,
) -> usize {
    if line_end < haystack.len() {
        line_content_end(haystack, line_start, line_end)
    } else {
        line_end
    }
}

/// The lines of `bytes`, as `str::lines` splits them.
#[cfg(feature = "cli")]
#[inline]
pub(crate) fn count_lines(bytes: &[u8]) -> usize {
    let unended = bytes.last().is_some_and(|&byte| byte != b'\n');
    memchr::memchr_iter(b'\n', bytes).count() + usize::from(unended)
}

/// Visits literal-prefilter candidate lines until `visit` returns `false`.
///
/// `matcher` must be a literal line matcher, which cannot fail. `visit` receives the match
/// kind, the line start, and the exclusive content end.
#[inline]
pub(crate) fn visit_candidate_lines(
    matcher: &RegexMatcher,
    haystack: &[u8],
    mut visit: impl FnMut(LineMatchKind, usize, usize) -> bool,
) {
    let mut offset = 0;
    while offset < haystack.len() {
        let Some(kind) = matcher
            .find_candidate_line(&haystack[offset..])
            .expect("literal line matcher should not fail")
        else {
            break;
        };
        let position = match kind {
            LineMatchKind::Confirmed(position) | LineMatchKind::Candidate(position) => position,
        };
        let line_start = offset
            + memrchr(b'\n', &haystack[offset..offset + position]).map_or(0, |index| index + 1);
        let line_end = line_end_from(haystack, line_start);
        if !visit(
            kind,
            line_start,
            line_content_end(haystack, line_start, line_end),
        ) || line_end == haystack.len()
        {
            break;
        }
        offset = line_end + 1;
    }
}

pub(crate) struct LineCounter<'a> {
    haystack: &'a [u8],
    line_end: usize,
    count: usize,
}

impl<'a> LineCounter<'a> {
    #[inline]
    pub(crate) fn new(haystack: &'a [u8]) -> Self {
        Self {
            haystack,
            line_end: 0,
            count: 0,
        }
    }

    /// Records a match start no smaller than the previous one. Returns whether it starts a
    /// newly counted line.
    #[inline]
    pub(crate) fn record(&mut self, start: usize) -> bool {
        start >= self.line_end && start < self.haystack.len() && self.record_valid_start(start)
    }

    /// Records a match start, which must be in bounds and no smaller than the previous one.
    /// Returns whether it starts a newly counted line.
    #[inline]
    pub(crate) fn record_valid_start(&mut self, start: usize) -> bool {
        debug_assert!(start < self.haystack.len());
        if start < self.line_end {
            return false;
        }

        // Only the matched line's end matters; unmatched lines between are never scanned.
        self.line_end = memchr(b'\n', &self.haystack[start..])
            .map_or(self.haystack.len(), |relative| start + relative + 1);
        self.count += 1;
        true
    }

    #[inline]
    pub(crate) fn count(self) -> usize {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LineCounter, build_line_matcher, line_content_end, line_end_from, visit_candidate_lines,
    };

    #[test]
    fn lines_end_at_their_line_feed_or_with_the_haystack() {
        let haystack = b"ab\n\ncd";

        let ends = [0, 2, 3, 4, 5, 6].map(|position| line_end_from(haystack, position));

        assert_eq!(ends, [2, 2, 3, 6, 6, 6]);
    }

    #[test]
    fn parallel_fold_matches_folding_each_byte() {
        // A two-thread pool runs the parallel branch on any machine.
        let threshold = super::profiles::PARALLEL_PASS_LEN;
        let chunk = super::profiles::PARALLEL_PASS_CHUNK;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("a two-thread pool builds");
        for len in [
            0,
            1,
            threshold - 1,
            threshold,
            threshold + chunk,
            threshold + 4_097,
        ] {
            let haystack = (0..=255_u8).cycle().take(len).collect::<Vec<_>>();

            let folded = pool.install(|| super::fold_ascii_bytes(&haystack));
            let parallel = super::fold_ascii_bytes_parallel(&haystack);

            let expected = haystack.to_ascii_lowercase();
            assert_eq!(folded, expected, "{len}");
            assert_eq!(parallel, expected, "{len}");
        }
    }

    #[test]
    fn candidate_lines_skip_unmatched_lines_trim_cr_and_stop_early() {
        let matcher = build_line_matcher(&[b"hit".to_vec()], crate::SearchConfig::default())
            .expect("literal matcher builds");
        let haystack = b"miss\nhit\r\n\nhit\r";
        let mut spans = Vec::new();
        visit_candidate_lines(&matcher, haystack, |_, start, end| {
            spans.push((start, end));
            true
        });
        assert_eq!(spans, [(5, 8), (11, 14)]);
        assert_eq!(line_content_end(haystack, 11, 15), 14);
        spans.clear();
        visit_candidate_lines(&matcher, haystack, |_, start, end| {
            spans.push((start, end));
            false
        });
        assert_eq!(spans, [(5, 8)]);
    }

    #[test]
    fn line_counter_matches_reference_for_every_subset_of_starts() {
        for haystack in [b"".as_slice(), b"\n", b"a\n\nb\r\nc", b"abc\n", b"abc"] {
            for subset in 0..(1_usize << haystack.len()) {
                let mut counter = LineCounter::new(haystack);
                let mut expected_lines = std::collections::BTreeSet::new();
                for start in 0..haystack.len() {
                    if subset & (1 << start) != 0 {
                        counter.record(start);
                        counter.record(start);
                        expected_lines.insert(
                            haystack[..start]
                                .iter()
                                .filter(|&&byte| byte == b'\n')
                                .count(),
                        );
                    }
                }
                counter.record(haystack.len());
                assert_eq!(counter.count(), expected_lines.len());
            }
        }
    }
}
