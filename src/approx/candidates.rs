use std::cmp::Ordering;
use std::ops::{Range, RangeInclusive};

use memchr::memchr;
use smallvec::SmallVec;

use super::{ApproximateCandidate, ApproximatePattern, PrefilterContext};
use crate::exact::{PairSample, for_each_mark, marks_differ_folded};
use crate::{Match, SearchConfig, fold_ascii, is_word_boundary, line_end_from};

/// The starts one region task reports, plus the margin it verifies to dedup ends exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StartRegion {
    owned: Range<usize>,
    /// `owned` widened by two on each side, since windows sharing an end start at most two
    /// bytes apart.
    verified: Range<usize>,
}

impl StartRegion {
    pub(super) fn new(owned: Range<usize>, haystack_len: usize) -> Self {
        Self {
            verified: owned.start.saturating_sub(2)..(owned.end + 2).min(haystack_len),
            owned,
        }
    }
}

/// Lazily finds the line end and `str::lines` content end (excluding a CRLF's CR) for each
/// candidate start, caching the current line.
///
/// Regions pass a haystack truncated to their windows' reach, so a long line isn't scanned
/// to its end by every region it crosses.
struct LineContentEnds<'a> {
    haystack: &'a [u8],
    /// A start and its line end; every start between them shares that line.
    line: Option<(usize, usize)>,
}

impl<'a> LineContentEnds<'a> {
    fn new(haystack: &'a [u8]) -> Self {
        Self {
            haystack,
            line: None,
        }
    }

    /// The position of the line feed ending `start`'s line, or the haystack length.
    #[inline]
    fn line_end(&mut self, start: usize) -> usize {
        match self.line {
            Some((from, line_end)) if (from..=line_end).contains(&start) => line_end,
            Some(_) | None => {
                let line_end = line_end_from(self.haystack, start);
                self.line = Some((start, line_end));
                line_end
            }
        }
    }

    #[inline]
    fn content_end(&mut self, start: usize) -> usize {
        let newline = self.line_end(start);
        if newline < self.haystack.len() && newline > 0 && self.haystack[newline - 1] == b'\r' {
            newline - 1
        } else {
            newline
        }
    }
}

/// The window starts implied by an unedited half found at `half_start`, `offset` bytes into
/// its pattern, or `None` if they would all precede the haystack.
///
/// A first half starts its window. Before a second half lies the edit, which shifts the
/// window start by at most one byte either way.
fn window_starts_around(half_start: usize, offset: usize) -> Option<RangeInclusive<usize>> {
    let shift = usize::from(offset != 0);
    let last = (half_start + shift).checked_sub(offset)?;
    Some(half_start.saturating_sub(offset + shift)..=last)
}

/// Maximum marks per pass; each adds a compare and select per vector.
const MAX_GROUP_MARKS: usize = 8;
/// Number of leading byte pairs of a half considered as its mark.
const MARK_CHOICES: usize = 6;

/// Bit 5 of each ASCII lowercase letter of `bytes` (at most [`PACKED_LEN`]), packed
/// little-endian. Set in a packed window, the bits make exactly those letters of `bytes`
/// compare equal in either case, since bit 5 alone tells an ASCII letter's cases apart.
fn either_case_bits(bytes: &[u8]) -> u128 {
    let mut bits = [0_u8; PACKED_LEN];
    for (bit, byte) in bits.iter_mut().zip(bytes) {
        *bit = u8::from(byte.is_ascii_lowercase()) << 5;
    }
    u128::from_le_bytes(bits)
}

/// Up to [`PACKED_LEN`] bytes packed little-endian into a `u128`, with a mask of the used
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackedBytes {
    bytes: u128,
    mask: u128,
    /// The [`either_case_bits`] of the bytes.
    either_case: u128,
}

impl PackedBytes {
    /// Packs `bytes`, or returns `None` if they don't fit.
    fn new(bytes: &[u8]) -> Option<Self> {
        let mut packed = [0_u8; PACKED_LEN];
        packed.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(Self {
            bytes: u128::from_le_bytes(packed),
            mask: LOW_BYTES[bytes.len()],
            either_case: either_case_bits(bytes),
        })
    }

    /// Whether `window` starts with the bytes, which hold no uppercase letter if `FOLD`:
    /// their letters then match in either case.
    #[inline(always)]
    fn starts<const FOLD: bool>(self, window: u128) -> bool {
        let window = if FOLD {
            window | self.either_case
        } else {
            window
        };
        (window ^ self.bytes) & self.mask == 0
    }
}

/// The [`PACKED_LEN`] bytes at `start` as a little-endian `u128`, or `None` near the end.
#[inline(always)]
fn packed_window(haystack: &[u8], start: usize) -> Option<u128> {
    haystack
        .get(start..)?
        .first_chunk::<PACKED_LEN>()
        .map(|window| u128::from_le_bytes(*window))
}

/// A haystack as the passes read it: as it is, or with `FOLD`, ignoring the ASCII case of
/// the letters of patterns, which are then stored lowered. Case is ignored as bytes are
/// compared, which costs less than writing a lowered copy of the haystack first, and `FOLD`
/// is a type parameter so that text read as it is pays nothing for the other kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SearchedText<'a, const FOLD: bool> {
    pub(super) bytes: &'a [u8],
}

impl<const FOLD: bool> SearchedText<'_, FOLD> {
    /// Whether `needle` is read at `start`.
    fn holds_at(self, start: usize, needle: &[u8]) -> bool {
        let Some(read) = self
            .bytes
            .get(start..)
            .and_then(|rest| rest.get(..needle.len()))
        else {
            return false;
        };
        if FOLD {
            read.iter()
                .copied()
                .map(fold_ascii)
                .eq(needle.iter().copied())
        } else {
            read == needle
        }
    }
}

/// A pattern half and its mark: two adjacent bytes of the half that a pass scans for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MarkedHalf {
    pattern_index: usize,
    /// Start of the half in its pattern.
    offset: usize,
    len: usize,
    /// `None` if the half is too long to pack.
    packed: Option<PackedBytes>,
    mark: [u8; 2],
    /// Start of the mark in the half.
    mark_offset: usize,
}

impl MarkedHalf {
    /// Whether the half is read in `text` at `half_start`.
    #[inline(always)]
    fn lies_at<const FOLD: bool>(
        &self,
        text: SearchedText<'_, FOLD>,
        half_start: usize,
        pattern: &[u8],
    ) -> bool {
        match (self.packed.as_ref(), packed_window(text.bytes, half_start)) {
            (Some(packed), Some(window)) => packed.starts::<FOLD>(window),
            (Some(_), None) | (None, _) => {
                text.holds_at(half_start, &pattern[self.offset..self.offset + self.len])
            }
        }
    }
}

/// The pattern halves, grouped into one scan pass per group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct MarkPlan {
    /// Within a group, marks have distinct first bytes; at most [`MAX_GROUP_MARKS`] each.
    groups: Vec<Vec<MarkedHalf>>,
}

/// A candidate mark for a half. Derived `Ord` compares fields in order, so the rarest pair
/// sorts first, then the earliest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MarkChoice {
    /// Occurrences in the sample, or 0 without one.
    stops: usize,
    /// Start of the pair in the half.
    offset: usize,
    mark: [u8; 2],
}

/// A half that has yet to get its mark.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnmarkedHalf<'a> {
    range: Range<usize>,
    bytes: &'a [u8],
    /// Candidate marks, rarest first.
    choices: SmallVec<[MarkChoice; MARK_CHOICES]>,
}

impl MarkPlan {
    /// Chooses a mark for every half: among its first [`MARK_CHOICES`] byte pairs, the one
    /// rarest in `sample` (or the first, without a sample).
    ///
    /// Both halves of a pattern join the first group with room where each can get a mark
    /// whose first byte is unique in the group, so one pass finds both in near order.
    /// Halves that can't (like the two of `aaaa`) get a group each.
    ///
    /// # Arguments
    ///
    /// * `patterns` - Normalized patterns
    /// * `sample` - A sample of the haystack, if it is long enough to repay one
    ///
    /// # Returns
    ///
    /// The plan, empty if no pattern has halves
    pub(super) fn new(patterns: &[ApproximatePattern], sample: Option<&PairSample>) -> Self {
        let mut plan = Self::default();
        for (pattern_index, pattern) in patterns.iter().enumerate() {
            let Some(halves) = &pattern.halves else {
                continue;
            };
            let halves = halves.clone().map(|range| {
                let bytes = &pattern.bytes[range.clone()];
                // `windows(2)` yields each adjacent byte pair as a slice.
                let mut choices = bytes
                    .windows(2)
                    .take(MARK_CHOICES)
                    .enumerate()
                    .map(|(offset, pair)| {
                        let mark = [pair[0], pair[1]];
                        MarkChoice {
                            stops: sample.map_or(0, |sample| sample.occurrences(mark)),
                            offset,
                            mark,
                        }
                    })
                    .collect::<SmallVec<[MarkChoice; MARK_CHOICES]>>();
                choices.sort_unstable();
                UnmarkedHalf {
                    range,
                    bytes,
                    choices,
                }
            });
            if !plan.place(pattern_index, &halves) {
                for half in &halves {
                    let placed = plan.place(pattern_index, std::slice::from_ref(half));
                    debug_assert!(placed, "a new group takes any one half");
                }
            }
        }
        plan
    }

    /// Places `halves` of one pattern together in the first group (or a new one) where each
    /// gets a mark with a unique first byte. Returns `false` if no group can take them.
    fn place(&mut self, pattern_index: usize, halves: &[UnmarkedHalf<'_>]) -> bool {
        let marks_in = |group: &[MarkedHalf]| {
            if group.len() + halves.len() > MAX_GROUP_MARKS {
                return None;
            }
            let mut chosen = SmallVec::<[MarkChoice; 2]>::new();
            for half in halves {
                let taken = |mark: [u8; 2]| {
                    let marks = group.iter().map(|half| half.mark);
                    let chosen = chosen.iter().map(|choice| choice.mark);
                    marks.chain(chosen).any(|other| other[0] == mark[0])
                };
                // Rejects the group when every pair of the half is taken.
                let choice = *half.choices.iter().find(|choice| !taken(choice.mark))?;
                chosen.push(choice);
            }
            Some(chosen)
        };
        let placed = self
            .groups
            .iter()
            .enumerate()
            .find_map(|(index, group)| marks_in(group).map(|chosen| (index, chosen)))
            .or_else(|| marks_in(&[]).map(|chosen| (self.groups.len(), chosen)));
        let Some((group, chosen)) = placed else {
            return false;
        };
        if group == self.groups.len() {
            self.groups.push(Vec::with_capacity(MAX_GROUP_MARKS));
        }
        for (half, choice) in halves.iter().zip(chosen) {
            self.groups[group].push(MarkedHalf {
                pattern_index,
                offset: half.range.start,
                len: half.bytes.len(),
                packed: PackedBytes::new(half.bytes),
                mark: choice.mark,
                mark_offset: choice.offset,
            });
        }
        true
    }

    pub(super) fn groups(&self) -> &[Vec<MarkedHalf>] {
        &self.groups
    }

    /// The halves if the plan has exactly one group, or `None`.
    pub(super) fn into_single_group(self) -> Option<Vec<MarkedHalf>> {
        // Converting to `[_; 1]` succeeds only for a Vec of length one.
        let [group] = <[Vec<MarkedHalf>; 1]>::try_from(self.groups).ok()?;
        Some(group)
    }

    /// Number of byte pairs [`Self::new`] would count in a sample for `patterns`, a measure
    /// of the cost of choosing marks.
    pub(super) fn choices(patterns: &[ApproximatePattern]) -> usize {
        let halves = patterns
            .iter()
            .filter_map(|pattern| pattern.halves.as_ref());
        halves
            .flatten()
            .map(|half| (half.len() - 1).min(MARK_CHOICES))
            .sum()
    }

    /// Whether the mark pass would stop more than once per `spacing` bytes of `sample`,
    /// counting each half's mark separately.
    pub(super) fn stops_within(&self, spacing: usize, sample: &PairSample) -> bool {
        let halves = self.groups.iter().flatten();
        let stops = halves
            .map(|half| sample.occurrences(half.mark))
            .sum::<usize>();
        stops * spacing > sample.counted_positions()
    }
}

/// Calls `visit(half_start, half)` for every half of `group` that is read in `text` with its
/// mark in `scan`, in mark order.
///
/// `visit` is `dyn` so the per-mark-count passes share one compiled copy of it.
fn for_each_group_half<const FOLD: bool>(
    text: SearchedText<'_, FOLD>,
    scan: Range<usize>,
    patterns: &[ApproximatePattern],
    group: &[MarkedHalf],
    visit: &mut dyn FnMut(usize, &MarkedHalf),
) {
    /// Monomorphized per mark count so the mark compares unroll.
    fn pass<const MARKS: usize, const FOLD: bool>(
        text: SearchedText<'_, FOLD>,
        scan: Range<usize>,
        patterns: &[ApproximatePattern],
        group: &[MarkedHalf],
        visit: &mut dyn FnMut(usize, &MarkedHalf),
    ) {
        /// Bytes lowered at a time, few enough to stay in the first-level cache.
        const LOWERED_BLOCK_LEN: usize = 4096;

        let marks: [[u8; 2]; MARKS] = std::array::from_fn(|half| group[half].mark);
        let mut visit_mark = |position: usize, half: usize| {
            let half = &group[half];
            // `if let ... && ...` requires both the pattern match and the condition.
            if let Some(half_start) = position.checked_sub(half.mark_offset)
                && half.lies_at(text, half_start, &patterns[half.pattern_index].bytes)
            {
                visit(half_start, half);
            }
        };
        // Marks without a letter stand in the text as written, whatever its case.
        let has_letter = marks.iter().flatten().any(u8::is_ascii_alphabetic);
        if !FOLD || !has_letter {
            for_each_mark::<MARKS, false>(&text.bytes[scan.clone()], &marks, |position, half| {
                visit_mark(scan.start + position, half);
            });
            return;
        }
        if marks_differ_folded(&marks) {
            for_each_mark::<MARKS, true>(&text.bytes[scan.clone()], &marks, |position, half| {
                visit_mark(scan.start + position, half);
            });
            return;
        }
        // The compare cannot tell these marks apart in either case, so it scans lowered
        // copies of blocks. A mark is two bytes, so a block starts with the last byte of
        // the one before, and a last byte alone starts none.
        let mut lowered = [0_u8; LOWERED_BLOCK_LEN];
        let mut block_start = scan.start;
        while block_start + 1 < scan.end {
            let raw = &text.bytes[block_start..scan.end.min(block_start + LOWERED_BLOCK_LEN)];
            let block = &mut lowered[..raw.len()];
            for (folded, &byte) in block.iter_mut().zip(raw) {
                *folded = fold_ascii(byte);
            }
            for_each_mark::<MARKS, false>(block, &marks, |position, half| {
                visit_mark(block_start + position, half);
            });
            block_start += raw.len() - 1;
        }
    }
    match group.len() {
        1 => pass::<1, FOLD>(text, scan, patterns, group, visit),
        2 => pass::<2, FOLD>(text, scan, patterns, group, visit),
        3 => pass::<3, FOLD>(text, scan, patterns, group, visit),
        4 => pass::<4, FOLD>(text, scan, patterns, group, visit),
        5 => pass::<5, FOLD>(text, scan, patterns, group, visit),
        6 => pass::<6, FOLD>(text, scan, patterns, group, visit),
        7 => pass::<7, FOLD>(text, scan, patterns, group, visit),
        8 => pass::<8, FOLD>(text, scan, patterns, group, visit),
        other => unreachable!("a group holds one to {MAX_GROUP_MARKS} halves, not {other}"),
    }
}

/// The scan range for `group`'s marks that finds every half implying a start in `starts`.
fn mark_scan_range(haystack: &[u8], starts: &Range<usize>, group: &[MarkedHalf]) -> Range<usize> {
    let farthest_mark = group.iter().fold(0, |farthest, half| {
        farthest.max(half.offset + half.mark_offset)
    });
    // A mark never precedes its window start; a half lies at most one byte late, and a
    // mark is two bytes.
    starts.start..haystack.len().min(starts.end + farthest_mark + 2)
}

/// How far windows starting before `starts_end` can reach, plus two bytes of slack: where
/// a [`LineContentEnds`] for those starts may stop reading.
fn window_reach(haystack: &[u8], starts_end: usize, patterns: &[ApproximatePattern]) -> usize {
    let longest = patterns.iter().map(|pattern| pattern.bytes.len()).max();
    haystack.len().min(starts_end + longest.unwrap_or(0) + 2)
}

/// Visits `(length, distance)` for windows read at `start` within one edit of `pattern`,
/// using at most `available` bytes.
///
/// For a second half (`first_half_len != 0`), starts holding the unedited first half are
/// skipped, since the first half's mark already finds them.
#[inline(always)]
fn for_each_window_at<const FOLD: bool>(
    pattern: &ApproximatePattern,
    text: SearchedText<'_, FOLD>,
    (start, available): (usize, usize),
    first_half_len: usize,
    visit: impl FnMut(usize, u32),
) {
    match (pattern.packed.as_ref(), packed_window(text.bytes, start)) {
        (Some(packed), Some(window)) => {
            if first_half_len != 0 && packed.starts_window::<FOLD>(window, first_half_len) {
                return;
            }
            packed.for_each_window_within_one_edit::<FOLD>(window, available, visit);
        }
        (Some(_), None) | (None, _) => {
            if first_half_len != 0 && text.holds_at(start, &pattern.bytes[..first_half_len]) {
                return;
            }
            let window = &text.bytes[start..start + available];
            for_each_window_within_one_edit::<FOLD>(&pattern.bytes, window, visit);
        }
    }
}

/// Lines counted in one or more consecutive regions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CountedLines {
    count: usize,
    /// Window starts that counted the first and last lines, if any.
    starts: Option<(usize, usize)>,
}

impl CountedLines {
    /// Counts the line of the window at `start`, which follows all counted lines.
    fn push(&mut self, start: usize) {
        self.count += 1;
        self.starts = Some((self.starts.map_or(start, |(first, _)| first), start));
    }

    /// Adds the lines counted in the following region. A line counted in both (no line feed
    /// between the two starts) counts once.
    ///
    /// # Arguments
    ///
    /// * `next` - The lines counted in the next region
    /// * `haystack` - The input the regions are of
    pub(super) fn append(&mut self, next: Self, haystack: &[u8]) {
        let Some((next_first, next_last)) = next.starts else {
            return;
        };
        let shared = self
            .starts
            .is_some_and(|(_, last)| memchr(b'\n', &haystack[last..next_first]).is_none());
        self.count += next.count - usize::from(shared);
        self.starts = Some((
            self.starts.map_or(next_first, |(first, _)| first),
            next_last,
        ));
    }

    pub(super) fn count(self) -> usize {
        self.count
    }
}

/// Counts lines with a window within one edit of a pattern starting in `owned`, where
/// windows stay within their `str::lines` line.
///
/// One pass finds all marks of `group` (the plan's single group) and verifies the starts
/// around each half. The first matching window settles its line, so the rest of that line
/// is skipped. `owned` must not be empty.
pub(super) fn count_marked_lines<const FOLD: bool>(
    text: SearchedText<'_, FOLD>,
    owned: Range<usize>,
    patterns: &[ApproximatePattern],
    group: &[MarkedHalf],
) -> CountedLines {
    let haystack = text.bytes;
    let scan = mark_scan_range(haystack, &owned, group);
    let mut lines = LineContentEnds::new(&haystack[..window_reach(haystack, owned.end, patterns)]);
    let mut counted = CountedLines::default();
    // Starts before this one lie in lines already counted.
    let mut resume = 0;
    for_each_group_half(text, scan, patterns, group, &mut |half_start, half| {
        if half_start + half.len < resume {
            return;
        }
        let pattern = &patterns[half.pattern_index];
        let Some(starts) = window_starts_around(half_start, half.offset) else {
            return;
        };
        let first = (*starts.start()).max(owned.start).max(resume);
        for start in first..=(*starts.end()).min(owned.end - 1) {
            let available = lines
                .content_end(start)
                .saturating_sub(start)
                .min(pattern.bytes.len() + 1);
            let mut within_one_edit = false;
            for_each_window_at(
                pattern,
                text,
                (start, available),
                half.offset,
                |_length, _edit_distance| within_one_edit = true,
            );
            if within_one_edit {
                counted.push(start);
                // If the line runs past the cursor's truncated haystack, this skips the
                // rest of the region.
                resume = lines.line_end(start) + 1;
                return;
            }
        }
    });
    counted
}

/// Windows of one pattern found in a region.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FoundWindows {
    /// Windows in discovery order: nearly sorted by `approximate_candidate_cmp` for halves
    /// in one pass, or two nearly sorted runs for halves split across two passes.
    windows: Vec<ApproximateCandidate>,
    /// First start not yet verified for a second half.
    next_second_start: usize,
}

/// Finds windows within one edit of each pattern that start in `region`.
///
/// One pass per group finds the marks, and the starts around each half are verified
/// immediately while the text is in cache. Returns, per pattern, the windows sorted by
/// `approximate_candidate_cmp`, deduplicated by end and filtered for whole words if set.
pub(super) fn marked_candidates(
    ctx: &PrefilterContext<'_>,
    region: &StartRegion,
    patterns: &[ApproximatePattern],
    plan: &MarkPlan,
) -> SmallVec<[Vec<ApproximateCandidate>; 8]> {
    if ctx.folds {
        marked_text_candidates::<true>(ctx, region, patterns, plan)
    } else {
        marked_text_candidates::<false>(ctx, region, patterns, plan)
    }
}

/// [`marked_candidates`] with the haystack read lowered if `FOLD`, which must be the
/// context's `folds`.
fn marked_text_candidates<const FOLD: bool>(
    ctx: &PrefilterContext<'_>,
    region: &StartRegion,
    patterns: &[ApproximatePattern],
    plan: &MarkPlan,
) -> SmallVec<[Vec<ApproximateCandidate>; 8]> {
    let haystack = ctx.haystack;
    let text = SearchedText::<FOLD> { bytes: haystack };
    let mut found = patterns
        .iter()
        .map(|_| FoundWindows::default())
        .collect::<SmallVec<[_; 8]>>();
    let reach = window_reach(haystack, region.verified.end, patterns);
    let mut line_ends = ctx
        .line_local
        .then(|| LineContentEnds::new(&ctx.haystack[..reach]));
    for group in plan.groups() {
        let scan = mark_scan_range(haystack, &region.verified, group);
        for_each_group_half(text, scan, patterns, group, &mut |half_start, half| {
            let pattern = &patterns[half.pattern_index];
            let Some(starts) = window_starts_around(half_start, half.offset) else {
                return;
            };
            let of_pattern = &mut found[half.pattern_index];
            let last = (*starts.end()).min(region.verified.end - 1);
            let mut first = (*starts.start()).max(region.verified.start);
            if half.offset != 0 {
                // Second halves up to two bytes apart share starts, each verified once.
                first = first.max(of_pattern.next_second_start);
                of_pattern.next_second_start = of_pattern.next_second_start.max(last + 1);
            }
            for start in first..=last {
                let window_limit = line_ends
                    .as_mut()
                    .map_or(haystack.len(), |line_ends| line_ends.content_end(start));
                let available = window_limit
                    .saturating_sub(start)
                    .min(pattern.bytes.len() + 1);
                for_each_window_at(
                    pattern,
                    text,
                    (start, available),
                    half.offset,
                    |length, edit_distance| {
                        of_pattern.windows.push(ApproximateCandidate::new(
                            start,
                            start + length,
                            half.pattern_index,
                            edit_distance,
                        ));
                    },
                );
            }
        });
    }
    found
        .into_iter()
        .map(|of_pattern| {
            let mut candidates = of_pattern.windows;
            sort_nearly_sorted(&mut candidates);
            dedup_ends_in_start_order(&mut candidates);
            // Sorted by start, so margin candidates form a short prefix and suffix.
            candidates.truncate(candidates.partition_point(|c| c.matched.start < region.owned.end));
            candidates
                .drain(..candidates.partition_point(|c| c.matched.start < region.owned.start));
            if ctx.config.whole_word() {
                candidates
                    .retain(|c| is_word_boundary(ctx.haystack, c.matched.start, c.matched.end));
            }
            candidates
        })
        .collect()
}

/// K-way merges sorted candidate lists (one candidate per start each), keeping the best
/// candidate per start.
pub(super) fn merge_sorted_runs(
    mut heads: SmallVec<[&[ApproximateCandidate]; 8]>,
) -> Vec<ApproximateCandidate> {
    heads.retain(|list| !list.is_empty());
    let mut merged = Vec::with_capacity(heads.iter().map(|list| list.len()).sum());
    while heads.len() > 1 {
        let start = heads
            .iter()
            .map(|list| list[0].matched.start)
            .min()
            .expect("at least two lists remain");
        let mut best: Option<ApproximateCandidate> = None;
        let mut emptied = false;
        for list in &mut heads {
            if list[0].matched.start != start {
                continue;
            }
            let candidate = list[0];
            // Ties go to the earlier list, like `Iterator::min_by`.
            if best.is_none_or(|best| approximate_candidate_cmp(&candidate, &best).is_lt()) {
                best = Some(candidate);
            }
            *list = &list[1..];
            emptied |= list.is_empty();
        }
        merged.push(best.expect("some head starts at the lowest start"));
        if emptied {
            heads.retain(|list| !list.is_empty());
        }
    }
    if let Some(rest) = heads.first() {
        merged.extend_from_slice(rest);
    }
    merged
}

/// Selects up to `limit` matches from sorted candidates, respecting overlap config.
pub(super) fn select_approximate_matches(
    candidates: impl IntoIterator<Item = ApproximateCandidate>,
    config: SearchConfig,
    limit: usize,
) -> Vec<Match> {
    let overlapping = config.overlapping();
    let candidates = candidates.into_iter();
    if overlapping {
        return candidates
            .take(limit)
            .map(|candidate| candidate.matched)
            .collect();
    }
    let cap = limit.min(256);
    let mut matches = Vec::with_capacity(cap);
    let mut last_end = 0_usize;

    for candidate in candidates {
        if candidate.matched.start < last_end {
            continue;
        }

        last_end = candidate.matched.end;
        matches.push(candidate.matched);

        if matches.len() == limit {
            break;
        }
    }

    matches
}

/// Appends the non-overlapping selection of one region's `candidates`, given that the
/// previous selection ended at `last_end`.
///
/// `speculative` is the region's own selection, ignoring earlier regions. Greedy selection
/// after a pick depends only on that pick, so once the replay picks a match also in
/// `speculative`, the rest of it is copied wholesale. Periodic text may stay out of phase
/// and replay the whole region.
pub(super) fn stitch_region_selection(
    matches: &mut Vec<Match>,
    last_end: &mut usize,
    candidates: &[ApproximateCandidate],
    speculative: &[Match],
) {
    let mut speculative_index = 0;
    for candidate in candidates {
        let matched = candidate.matched;
        if matched.start < *last_end {
            continue;
        }
        speculative_index +=
            speculative[speculative_index..].partition_point(|picked| picked.start < matched.start);
        if speculative.get(speculative_index) == Some(&matched) {
            matches.extend_from_slice(&speculative[speculative_index..]);
            *last_end = speculative.last().map_or(*last_end, |picked| picked.end);
            return;
        }
        matches.push(matched);
        *last_end = matched.end;
    }
}

/// Compares candidates by (start ASC, edit_distance ASC, length DESC, pattern_index ASC, end ASC).
#[inline]
pub(super) fn approximate_candidate_cmp(
    left: &ApproximateCandidate,
    right: &ApproximateCandidate,
) -> Ordering {
    left.matched
        .start
        .cmp(&right.matched.start)
        .then_with(|| left.edit_distance.cmp(&right.edit_distance))
        .then_with(|| {
            let left_len = left.matched.end - left.matched.start;
            let right_len = right.matched.end - right.matched.start;
            right_len.cmp(&left_len)
        })
        .then_with(|| left.matched.pattern_index.cmp(&right.matched.pattern_index))
        .then_with(|| left.matched.end.cmp(&right.matched.end))
}

/// Sorts by `approximate_candidate_cmp` with insertion sort, which is linear when each
/// candidate is only a few places out of order (the usual case). If the moves exceed a
/// per-candidate budget, it falls back to the stable sort, which bounds the cost at
/// O(n log n) and merges presorted runs in linear time.
pub(super) fn sort_nearly_sorted(candidates: &mut [ApproximateCandidate]) {
    const NEARLY_SORTED_MOVES_PER_CANDIDATE: usize = 8;

    let mut move_budget = NEARLY_SORTED_MOVES_PER_CANDIDATE * candidates.len();
    for index in 1..candidates.len() {
        let mut position = index;
        while position > 0
            && approximate_candidate_cmp(&candidates[position], &candidates[position - 1]).is_lt()
        {
            candidates.swap(position, position - 1);
            position -= 1;
        }
        // Checked once per candidate, so one insertion may overshoot the budget.
        let Some(remaining) = move_budget.checked_sub(index - position) else {
            candidates.sort_by(approximate_candidate_cmp);
            return;
        };
        move_budget = remaining;
    }
}

/// Keeps, per end, the candidate with the lowest edit distance (ties go to the later,
/// shorter window), preserving order.
///
/// `candidates` must be sorted by start. Windows sharing an end start at most two bytes
/// apart, so a tiny table indexed by `end` holds every live rival in one linear pass.
pub(super) fn dedup_ends_in_start_order(candidates: &mut Vec<ApproximateCandidate>) {
    const RECENT_END_SLOTS: usize = 8;
    const DROPPED: u32 = u32::MAX;

    let mut recent: [Option<usize>; RECENT_END_SLOTS] = [None; RECENT_END_SLOTS];
    for index in 0..candidates.len() {
        let end = candidates[index].matched.end;
        let slot = &mut recent[end % RECENT_END_SLOTS];
        if let Some(rival) = *slot
            && candidates[rival].matched.end == end
        {
            // The rival starts earlier, so this window is shorter and wins distance ties.
            if candidates[index].edit_distance <= candidates[rival].edit_distance {
                candidates[rival].edit_distance = DROPPED;
            } else {
                candidates[index].edit_distance = DROPPED;
                continue;
            }
        }
        *slot = Some(index);
    }
    candidates.retain(|candidate| candidate.edit_distance != DROPPED);
}

/// Splits a pattern into two halves, one of which any window within one edit contains
/// unedited (pigeonhole principle).
///
/// Returns `None` for patterns shorter than four bytes or an edit distance other than one.
pub(super) fn pattern_halves(pattern: &[u8], max_edit_distance: u32) -> Option<[Range<usize>; 2]> {
    let split = pattern.len() / 2;
    (max_edit_distance == 1 && pattern.len() >= 4).then_some([0..split, split..pattern.len()])
}

/// Bytes compared at once as a `u128`.
const PACKED_LEN: usize = 16;

/// `LOW_BYTES[n]` has its lowest `n` bytes set; entries from [`PACKED_LEN`] on are all ones.
/// The power-of-two length lets callers index by `% len` to elide bounds checks.
const LOW_BYTES: [u128; 2 * PACKED_LEN] = {
    let mut masks = [u128::MAX; 2 * PACKED_LEN];
    // `for` loops aren't allowed in const evaluation.
    let mut count = 0;
    while count < PACKED_LEN {
        masks[count] = (1 << (8 * count)) - 1;
        count += 1;
    }
    masks
};

/// A pattern packed into integers for branch-free comparison with a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PackedPattern {
    len: usize,
    /// The pattern, little-endian: aligned with an unedited or substituted window.
    aligned: u128,
    /// Shifted down a byte: aligned with a window missing a byte.
    lowered: u128,
    /// Shifted up a byte: aligned with a window with an extra byte.
    raised: u128,
    /// Masks for windows one shorter, equal to, and one longer than the pattern.
    shorter: u128,
    equal: u128,
    longer: u128,
    /// The [`either_case_bits`] of the pattern, aligned like `aligned`.
    aligned_either_case: u128,
    /// The same bits, aligned like `lowered`.
    lowered_either_case: u128,
    /// The same bits, aligned like `raised`.
    raised_either_case: u128,
}

impl PackedPattern {
    /// Packs a pattern, or returns `None` if it is empty or leaves no room for a window one
    /// byte longer.
    pub(super) fn new(pattern: &[u8]) -> Option<Self> {
        let len = pattern.len();
        if len == 0 || len >= PACKED_LEN {
            return None;
        }
        let PackedBytes {
            bytes: aligned,
            either_case,
            ..
        } = PackedBytes::new(pattern)?;
        Some(Self {
            len,
            aligned,
            lowered: aligned >> 8,
            raised: aligned << 8,
            shorter: LOW_BYTES[len - 1],
            equal: LOW_BYTES[len],
            longer: LOW_BYTES[len + 1],
            aligned_either_case: either_case,
            lowered_either_case: either_case >> 8,
            raised_either_case: either_case << 8,
        })
    }

    /// Whether `window` starts with the first `len` bytes of the pattern, in either case
    /// if `FOLD`.
    #[inline(always)]
    fn starts_window<const FOLD: bool>(&self, window: u128, len: usize) -> bool {
        let window = if FOLD {
            window | self.aligned_either_case
        } else {
            window
        };
        // `%` elides the bounds check; `len` is at most the pattern's.
        (window ^ self.aligned) & LOW_BYTES[len % LOW_BYTES.len()] == 0
    }

    /// Packed version of [`for_each_window_within_one_edit`].
    ///
    /// The common prefix length comes from one `trailing_zeros`. A window is within one edit
    /// if nothing differs past the first difference once the pattern is shifted for the
    /// edit: unshifted for a substitution, down for a deletion, up for an insertion.
    ///
    /// With `FOLD`, the pattern holds no uppercase letter and its letters match in either
    /// case.
    ///
    /// # Arguments
    ///
    /// * `window` - The bytes at the window start, little-endian
    /// * `available` - How many of them a window may use, fewer at a line or haystack end
    /// * `visit` - Receives `(length, edit_distance)` in `approximate_candidate_cmp` order
    #[inline(always)]
    pub(super) fn for_each_window_within_one_edit<const FOLD: bool>(
        &self,
        window: u128,
        available: usize,
        mut visit: impl FnMut(usize, u32),
    ) {
        // The window as each alignment of the pattern compares with it.
        let [aligned, raised, lowered] = if FOLD {
            [
                window | self.aligned_either_case,
                window | self.raised_either_case,
                window | self.lowered_either_case,
            ]
        } else {
            [window; 3]
        };
        let differing = (aligned ^ self.aligned) & self.equal;
        // Zero has 128 trailing zeros, so `common` is `PACKED_LEN` when nothing differs.
        let common = (differing.trailing_zeros() / u8::BITS) as usize;
        // Masks from the first difference on, and past it. `%` elides the bounds check.
        let from_edit = !LOW_BYTES[common % LOW_BYTES.len()];
        let past_edit = !LOW_BYTES[(common + 1) % LOW_BYTES.len()];
        if differing == 0 && available >= self.len {
            visit(self.len, 0);
        }
        if available > self.len && (raised ^ self.raised) & self.longer & past_edit == 0 {
            visit(self.len + 1, 1);
        }
        if differing != 0 && available >= self.len && differing & past_edit == 0 {
            visit(self.len, 1);
        }
        if available + 1 >= self.len && (lowered ^ self.lowered) & self.shorter & from_edit == 0 {
            visit(self.len - 1, 1);
        }
    }
}

/// Visits `(length, distance)` for each prefix of `window` of length `m - 1`, `m` or `m + 1`
/// within one edit of `pattern` (length `m`), in `approximate_candidate_cmp` order.
///
/// Past the common prefix, a length is within one edit only if the remainders match after
/// skipping one byte, so each length costs one suffix comparison. With `FOLD`, the pattern
/// holds no uppercase letter and the window's letters match in either case.
#[inline(always)]
pub(super) fn for_each_window_within_one_edit<const FOLD: bool>(
    pattern: &[u8],
    window: &[u8],
    mut visit: impl FnMut(usize, u32),
) {
    let pattern_len = pattern.len();
    let same_byte = |of_pattern: &u8, of_window: &u8| {
        *of_pattern
            == if FOLD {
                fold_ascii(*of_window)
            } else {
                *of_window
            }
    };
    let same = |of_pattern: &[u8], of_window: &[u8]| {
        of_pattern
            .iter()
            .zip(of_window)
            .all(|(left, right)| same_byte(left, right))
    };
    let common = pattern
        .iter()
        .zip(window)
        .take_while(|(left, right)| same_byte(left, right))
        .count();
    let unedited = common == pattern_len;

    if unedited {
        visit(pattern_len, 0);
    }
    if window.len() > pattern_len
        && (unedited || same(&pattern[common..], &window[common + 1..=pattern_len]))
    {
        visit(pattern_len + 1, 1);
    }
    if !unedited
        && window.len() >= pattern_len
        && same(&pattern[common + 1..], &window[common + 1..pattern_len])
    {
        visit(pattern_len, 1);
    }
    let shorter = pattern_len - 1;
    if window.len() >= shorter
        && (common >= shorter || same(&pattern[common + 1..], &window[common..shorter]))
    {
        visit(shorter, 1);
    }
}

#[cfg(test)]
mod tests {
    use triple_accel::levenshtein::levenshtein_naive_k;

    use super::*;
    use crate::approx::tests::random_below;

    fn raw(bytes: &[u8]) -> SearchedText<'_, false> {
        SearchedText { bytes }
    }

    fn patterns(texts: &[&str]) -> Vec<ApproximatePattern> {
        texts
            .iter()
            .map(|text| ApproximatePattern::new(text.as_bytes().to_vec(), 1))
            .collect()
    }

    fn chosen_marks(texts: &[&str], haystack: &[u8]) -> Vec<Vec<[u8; 2]>> {
        let sample = PairSample::new(haystack, false);
        MarkPlan::new(&patterns(texts), sample.as_ref())
            .groups()
            .iter()
            .map(|group| group.iter().map(|half| half.mark).collect())
            .collect()
    }

    #[test]
    fn halves_are_marked_by_the_pair_the_haystack_holds_least_often() {
        // `st` and `tu` are in every line and `tt` in none; `s=` is in every line too.
        let text =
            b"request status=200 status=500 user=7 stu and a good deal more text\n".repeat(2_000);

        let sampled = chosen_marks(&["sttus=500"], &text);
        let unsampled = chosen_marks(&["sttus=500"], b"too short to sample");
        let shifted = chosen_marks(&["EROR", "ERASED"], &text);

        assert_eq!(sampled, [vec![*b"tt", *b"=5"]]);
        // The first pair of each half, unless its first byte is taken.
        assert_eq!(unsampled, [vec![*b"st", *b"=5"]]);
        assert_eq!(shifted, [vec![*b"ER", *b"OR", *b"RA", *b"SE"]]);
    }

    #[test]
    fn halves_one_pass_cannot_tell_apart_or_has_no_room_for_open_another_group() {
        let text = b"a line of plain text that holds none of the patterns below\n".repeat(2_000);

        // The second half of `aaaa` is its first, and `ERORS` starts like `EROR`.
        let repeated = chosen_marks(&["aaaa"], &text);
        let alike = chosen_marks(&["EROR", "ERORS"], &text);
        let many = chosen_marks(&["abcd", "efgh", "ijkl", "mnop", "qrst"], &text);
        let short = chosen_marks(&["abc"], &text);

        assert_eq!(repeated, [vec![*b"aa"], vec![*b"aa"]]);
        assert_eq!(alike, [[*b"ER", *b"OR"], [*b"ER", *b"OR"]]);
        assert_eq!(many.iter().map(Vec::len).collect::<Vec<_>>(), [8, 2]);
        assert!(short.is_empty());
    }

    #[test]
    fn plans_hand_over_their_halves_only_if_one_pass_covers_them() {
        let single = MarkPlan::new(&patterns(&["abcd", "efgh"]), None);
        let several = MarkPlan::new(&patterns(&["aaaa"]), None);
        let none = MarkPlan::new(&patterns(&["abc"]), None);
        let expected = single.groups()[0].clone();

        assert_eq!(single.into_single_group(), Some(expected));
        assert_eq!(several.into_single_group(), None);
        assert_eq!(none.into_single_group(), None);
    }

    #[test]
    fn plans_tell_how_often_a_sample_holds_their_marks() {
        // Every third position starts `ab`, and none `xy`.
        let dense = b"ab ".repeat(40_000);
        let sample = PairSample::new(&dense, false).expect("the text is long enough to sample");

        let stops_often = MarkPlan::new(&patterns(&["abxy"]), Some(&sample));
        let stops_never = MarkPlan::new(&patterns(&["xyzw"]), None);
        let none = MarkPlan::new(&patterns(&["abc"]), Some(&sample));

        assert!(stops_often.stops_within(4, &sample) && !stops_often.stops_within(3, &sample));
        assert!(!stops_never.stops_within(usize::MAX, &sample));
        assert!(!none.stops_within(usize::MAX, &sample));
    }

    #[test]
    fn choosing_marks_counts_the_first_pairs_of_every_half() {
        // Halves of two and three bytes, a pattern without halves, and halves long enough
        // to hit the `MARK_CHOICES` cap.
        let short = MarkPlan::choices(&patterns(&["abcde", "abc"]));
        let long = MarkPlan::choices(&patterns(&["an unusually long one"]));

        assert_eq!(short, 1 + 2);
        assert_eq!(long, 2 * MARK_CHOICES);
    }

    #[test]
    fn packed_bytes_start_the_windows_that_hold_them_first() {
        let haystack = b"0123456789abcdefXYZ";
        let packed = PackedBytes::new(b"3456").expect("four bytes fit");
        let whole = PackedBytes::new(&haystack[..PACKED_LEN]).expect("a full window fits");

        let at = |start| packed_window(haystack, start);

        assert!(at(3).is_some_and(|window| packed.starts::<false>(window)));
        assert!(at(2).is_some_and(|window| !packed.starts::<false>(window)));
        assert!(at(0).is_some_and(|window| whole.starts::<false>(window)));
        assert_eq!(at(haystack.len() - PACKED_LEN + 1), None);
        assert_eq!(PackedBytes::new(&haystack[..PACKED_LEN + 1]), None);
    }

    #[test]
    fn packed_letters_match_in_either_case_only_when_folding() {
        // The bytes next to the letters differ from them in bit 5 too.
        let packed = PackedBytes::new(b"a@z[1").expect("five bytes fit");
        let window = |bytes: &[u8; PACKED_LEN]| u128::from_le_bytes(*bytes);
        let same = window(b"a@z[1...........");
        let other_case = window(b"A@Z[1...........");
        let other_bit_5 = [
            window(b"a`z[1..........."),
            window(b"a@z{1..........."),
            window(b"a@z[\x11..........."),
        ];

        let folding = [
            packed.starts::<true>(same),
            packed.starts::<true>(other_case),
        ];
        let exact = [
            packed.starts::<false>(same),
            packed.starts::<false>(other_case),
        ];
        let mistaken = other_bit_5.map(|window| packed.starts::<true>(window));

        assert_eq!(folding, [true, true]);
        assert_eq!(exact, [true, false]);
        assert_eq!(mistaken, [false; 3]);
    }

    #[test]
    fn folded_text_is_compared_lowered_byte_by_byte() {
        let haystack = b"xx NEEdle @[`{ Needle";
        let text = SearchedText::<true> { bytes: haystack };

        let held = [
            text.holds_at(3, b"needle"),
            text.holds_at(15, b"needle"),
            text.holds_at(10, b"`{`{"),
            text.holds_at(16, b"needle"),
            raw(haystack).holds_at(3, b"needle"),
        ];
        let mut windows = Vec::new();
        for_each_window_within_one_edit::<true>(b"needle", b"NEEdLE @", |length, distance| {
            windows.push((length, distance));
        });
        let mut other_bit_5 = Vec::new();
        for_each_window_within_one_edit::<true>(b"a[b@", b"A{B`", |length, distance| {
            other_bit_5.push((length, distance));
        });

        assert_eq!(held, [true, true, false, false, false]);
        assert_eq!(windows, [(6, 0), (7, 1), (5, 1)]);
        assert_eq!(other_bit_5, []);
    }

    #[test]
    fn second_halves_leave_a_start_to_the_first_half_that_lies_there() {
        let patterns = patterns(&[
            "abcdefgh",
            "an unusually long pattern, with halves too long to pack",
        ]);
        let windows_at = |haystack: &[u8], pattern: &ApproximatePattern, first_half_len| {
            let mut windows = Vec::new();
            let available = haystack.len().min(pattern.bytes.len() + 1);
            for_each_window_at(
                pattern,
                raw(haystack),
                (0, available),
                first_half_len,
                |length, distance| windows.push((length, distance)),
            );
            windows
        };
        // The second window has an edit in its first half.
        let short = [&b"abcdefgh and more text"[..], b"abXdefgh and more text"];
        let long_text = String::from_utf8_lossy(&patterns[1].bytes).into_owned();
        let long = [long_text.clone(), long_text.replacen('u', "U", 1)];

        for (pattern, [unedited, edited]) in [
            (&patterns[0], short.map(<[u8]>::to_vec)),
            (&patterns[1], long.map(String::into_bytes)),
        ] {
            let len = pattern.bytes.len();
            let half = len / 2;

            assert_eq!(windows_at(&unedited, pattern, half), []);
            assert_eq!(windows_at(&edited, pattern, half), [(len, 1)]);
            assert_eq!(windows_at(&edited, pattern, 0), [(len, 1)]);
            assert!(windows_at(&unedited, pattern, 0).starts_with(&[(len, 0)]));
        }
    }

    #[test]
    fn line_cursors_cut_at_the_reach_of_the_windows_end_no_window_short() {
        let patterns = patterns(&["EROR", "sttus=500"]);
        // Lines longer and shorter than a pattern, ends of both kinds, an empty line, and
        // a CR that ends the haystack.
        let haystack =
            b"a long line without an end in sight\nab\r\n\r\nEROR\r\nsttus=50\r".repeat(3);
        let longest = patterns.iter().map(|pattern| pattern.bytes.len()).max();
        let longest = longest.expect("there are patterns");

        for starts_end in 1..=haystack.len() {
            let reach = window_reach(&haystack, starts_end, &patterns);
            let mut whole = LineContentEnds::new(&haystack);
            let mut cut = LineContentEnds::new(&haystack[..reach]);

            for start in 0..starts_end {
                let available = |lines: &mut LineContentEnds<'_>| {
                    (lines.content_end(start).saturating_sub(start)).min(longest + 1)
                };
                assert_eq!(
                    available(&mut cut),
                    available(&mut whole),
                    "{start} {starts_end}"
                );
            }
        }
    }

    #[test]
    fn patterns_have_halves_from_four_bytes_on_at_one_edit() {
        let halves = [3, 4, 5, 8].map(|len| pattern_halves(&vec![b'a'; len], 1));
        let other_distances = [0, 2].map(|distance| pattern_halves(b"abcdefgh", distance));

        // An odd length leaves the second half the longer one.
        assert_eq!(
            halves,
            [
                None,
                Some([0..2, 2..4]),
                Some([0..2, 2..5]),
                Some([0..4, 4..8])
            ]
        );
        assert_eq!(other_distances, [None, None]);
    }

    #[test]
    fn marks_that_collide_in_either_case_are_found_in_lowered_blocks() {
        let patterns = patterns(&["@a@a@a", "`b`b`b"]);
        let plan = MarkPlan::new(&patterns, None);
        // Longer than a lowered block, so halves lie across the ends of blocks.
        let haystack = b"xx @A@a@A yy `B`b`B zz `b@a ".repeat(600);
        let lowered = haystack.to_ascii_lowercase();
        let group = &plan.groups()[0];
        let marks = group.iter().map(|half| half.mark).collect::<Vec<_>>();
        let scan = mark_scan_range(&haystack, &(0..haystack.len()), group);
        let halves_in =
            |found: &mut Vec<(usize, usize, usize)>, start: usize, half: &MarkedHalf| {
                found.push((start, half.pattern_index, half.offset));
            };

        let mut in_either_case = Vec::new();
        let text = SearchedText::<true> { bytes: &haystack };
        for_each_group_half(text, scan.clone(), &patterns, group, &mut |start, half| {
            halves_in(&mut in_either_case, start, half);
        });
        let mut as_lowered = Vec::new();
        for_each_group_half(raw(&lowered), scan, &patterns, group, &mut |start, half| {
            halves_in(&mut as_lowered, start, half);
        });

        // `@` and a backtick differ only in the bit that tells case apart.
        assert_eq!(plan.groups().len(), 1);
        assert_eq!(marks, [*b"@a", *b"a@", *b"`b", *b"b`"]);
        assert!(!marks_differ_folded(&[*b"@a", *b"a@", *b"`b", *b"b`"]));
        assert_eq!(in_either_case, as_lowered);
        assert!(in_either_case.len() >= 4 * 600);
    }

    #[test]
    fn halves_are_found_packed_and_byte_by_byte_up_to_the_end_of_the_haystack() {
        let patterns = patterns(&[
            "abcdefgh",
            "an unusually long pattern, with halves too long to pack",
        ]);
        let plan = MarkPlan::new(&patterns, None);
        let mut haystack = b"xx abcd xx efgh xx an unusually long pattern, xx".to_vec();
        haystack.extend_from_slice(b" with halves too long to pack efgh");
        let found_in = |haystack: &[u8]| {
            let mut found = Vec::new();
            for group in plan.groups() {
                let scan = mark_scan_range(haystack, &(0..haystack.len()), group);
                for_each_group_half(raw(haystack), scan, &patterns, group, &mut |start, half| {
                    found.push((start, half.pattern_index, half.offset));
                });
            }
            found
        };

        let found = found_in(&haystack);
        // Without its last byte the haystack ends inside the last half.
        let cut_short = found_in(&haystack[..haystack.len() - 1]);

        let expected = [(3, 0, 0), (11, 0, 4), (19, 1, 0), (49, 1, 27), (78, 0, 4)];
        assert_eq!(found, expected);
        assert_eq!(cut_short, expected[..4]);
    }

    #[test]
    fn windows_start_at_a_first_half_and_within_a_byte_of_where_a_second_half_puts_them() {
        let first_half = [0, 7].map(|half_start| window_starts_around(half_start, 0));
        let second_half = [1, 2, 3, 9].map(|half_start| window_starts_around(half_start, 3));

        assert_eq!(first_half, [Some(0..=0), Some(7..=7)]);
        // A second half one byte into the haystack leaves no room for the first.
        assert_eq!(second_half, [None, Some(0..=0), Some(0..=1), Some(5..=7)]);
    }

    #[test]
    fn lines_counted_in_consecutive_regions_add_up_with_a_shared_line_counted_once() {
        let haystack = b"ab\ncdefgh\nij\nkl\n";
        let region = |starts: &[usize]| {
            let mut counted = CountedLines::default();
            for &start in starts {
                counted.push(start);
            }
            counted
        };
        let mut total = CountedLines::default();

        // The second line has windows starting in the first and in the third region.
        for starts in [&[0, 3][..], &[], &[7, 10], &[13]] {
            total.append(region(starts), haystack);
        }

        assert_eq!(total.count(), 4);
        assert_eq!(total, region(&[0, 3, 10, 13]));
    }

    #[test]
    fn line_cursors_follow_starts_in_any_order() {
        let haystack = b"ab\r\ncd\nef\r";
        let mut lines = LineContentEnds::new(haystack);

        // Forwards, back into the first line, and into the last, which ends in a bare CR.
        let ends = [0, 5, 1, 8, 4].map(|start| (lines.line_end(start), lines.content_end(start)));

        assert_eq!(ends, [(3, 2), (6, 6), (3, 2), (10, 10), (6, 6)]);
    }

    /// Naive reference count of lines with a window within one edit of a pattern.
    fn lines_within_one_edit(patterns: &[ApproximatePattern], text: &[u8]) -> usize {
        let within_one_edit = |line: &str| {
            patterns.iter().any(|pattern| {
                let lengths = pattern.bytes.len() - 1..=pattern.bytes.len() + 1;
                (0..line.len()).any(|start| {
                    lengths.clone().any(|length| {
                        line.as_bytes()
                            .get(start..start + length)
                            .is_some_and(|window| {
                                levenshtein_naive_k(&pattern.bytes, window, 1).is_some()
                            })
                    })
                })
            })
        };
        std::str::from_utf8(text)
            .expect("the alphabet is ASCII")
            .lines()
            .filter(|line| within_one_edit(line))
            .count()
    }

    fn counted_in_regions(
        text: &[u8],
        region_len: usize,
        patterns: &[ApproximatePattern],
        group: &[MarkedHalf],
    ) -> usize {
        let mut counted = CountedLines::default();
        for start in (0..text.len()).step_by(region_len) {
            let owned = start..(start + region_len).min(text.len());
            counted.append(count_marked_lines(raw(text), owned, patterns, group), text);
        }
        counted.count()
    }

    #[test]
    fn marked_passes_count_the_lines_with_a_window_within_one_edit_whatever_the_regions() {
        let mut state = 0x243f_6a88_85a3_08d3_u64;
        let mut next = |bound| random_below(&mut state, bound);
        // `aaa` overlaps itself, and `=5` is a mark that starts inside its half.
        for texts in [&["EROR"][..], &["aaabcd", "EROR"], &["sttus=500", "aaabcd"]] {
            let patterns = patterns(texts);
            // Near misses and line separators, shuffled.
            let words = [
                "EROR",
                "ERROR",
                "ERR",
                "ROR",
                "aaaaabXd",
                "aaabcd",
                "aabcd",
                "sttus=50",
                "status=500",
                "=50",
                "st",
                "a",
                " ",
                " ",
                "\n",
                "\n",
                "\r\n",
                "\r",
            ];
            let text = (0..600)
                .map(|_| words[next(words.len() as u64) as usize])
                .collect::<String>()
                .into_bytes();
            let plan = MarkPlan::new(&patterns, None);
            let [group] = plan.groups() else {
                panic!("the halves differ: {texts:?}");
            };
            let expected = lines_within_one_edit(&patterns, &text);
            assert!(expected > 10, "{texts:?}");

            for region_len in [1, 2, 3, 7, 64, 1_000, text.len()] {
                assert_eq!(
                    counted_in_regions(&text, region_len, &patterns, group),
                    expected,
                    "{texts:?} in regions of {region_len}"
                );
            }
        }
    }

    #[test]
    fn marked_passes_count_the_lines_of_random_patterns_by_sampled_marks() {
        let mut state = 0xbb67_ae85_84ca_a73b_u64;
        let mut next = |bound| random_below(&mut state, bound);
        let mut planned = 0;
        let mut lines_with_a_window = 0;
        for round in 0..300 {
            // Packable and unpackable patterns, in haystacks from below `PACKED_LEN` to
            // long enough to sample.
            let patterns = (0..1 + next(2))
                .map(|_| {
                    let length = 4 + next(if round % 4 == 0 { 30 } else { 5 });
                    let bytes = (0..length).map(|_| b"abcd"[next(4) as usize]).collect();
                    ApproximatePattern::new(bytes, 1)
                })
                .collect::<Vec<_>>();
            let length = [9, 300, 1_200][round as usize % 3];
            let text = (0..length)
                .map(|_| b"aabbccdd\r\n"[next(10) as usize])
                .collect::<Vec<_>>();
            let sample = PairSample::new(&text, false);
            let plan = MarkPlan::new(&patterns, sample.as_ref());
            // Halves that one pass cannot tell apart are counted by a search.
            let [group] = plan.groups() else {
                continue;
            };
            planned += 1;
            let expected = lines_within_one_edit(&patterns, &text);
            lines_with_a_window += expected;

            for region_len in [1, 5, 64, text.len()] {
                assert_eq!(
                    counted_in_regions(&text, region_len, &patterns, group),
                    expected,
                    "{patterns:?} {text:?} {region_len}"
                );
            }
        }
        assert!(planned > 100 && lines_with_a_window > 300);
    }
}
