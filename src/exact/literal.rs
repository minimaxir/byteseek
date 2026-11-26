//! Block scanner for the occurrences of a few literal patterns.
//!
//! Unlike a search that exits its vector loop at every hit, this scanner compares a batch of
//! 64-byte blocks in straight-line code, keeps the flags of blocks holding a candidate, and
//! verifies them afterwards (sharing parts with [`super::pairs`]). Dense matches stay cheap.
//!
//! Per pattern it compares one to four bytes, its *probes*, chosen greedily from a sample of
//! the haystack to leave the fewest candidates. The compare loops are plain array loops that
//! the compiler auto-vectorizes for any target.

use std::cmp::Reverse;

use super::pairs::{
    BATCH, BLOCK, COUNTED_CHUNK_LEN, HEAD_LEN, Hits, Literal, MAX_SAMPLE_WINDOWS, PairSample,
    REJECTED_ALLOWANCE, SAMPLE_WINDOW_LEN, SampleWindow, count_bytes, rare_offsets,
};
use crate::{CaseMode, Match, window_matches_ascii_insensitive};

/// Largest pattern set scanned; each pattern adds compares to every vector.
const MAX_PATTERNS: usize = 8;
/// Longest pattern scanned, bounding the cost of verifying a rejected candidate.
const MAX_PATTERN_LEN: usize = 64;
/// Cap on estimated matches or lines, since a sample cannot vouch for unseen text.
const EXPECTED_CEILING: usize = 1 << 20;
/// Most bytes of a pattern compared per position.
const MAX_PROBES: usize = 4;
/// Shortest haystack whose scan repays sampling it.
const MIN_HAYSTACK_LEN: usize = 32 * 1024;
/// Positions per sampled window whose pattern head fits inside the window.
const COUNTED_LEN: usize = SAMPLE_WINDOW_LEN - HEAD_LEN;
/// Rarest pattern offsets considered for each probe after the first.
const RARE_OFFSETS: usize = 4;
/// Sampled positions per candidate that one more probe must remove to repay its compares.
/// Measured on four-letter text, a probe and a candidate cost about the same at this spacing.
const PROBE_REPAY_SPACING: usize = 4 * BLOCK;
/// Scanned bytes per allowed rejected candidate; rejecting more means the sample misjudged
/// the haystack, and the scan gives up.
const REJECTED_SPACING: usize = BLOCK / 4;
/// Blocks covering the tail left after the whole blocks: less than a block and a head.
const TAIL_BLOCKS: usize = 2;
/// Returned by a visitor to end the scan.
pub(crate) const STOP: usize = usize::MAX;

// Compile-time check: counted positions split into whole chunks.
const _: () = assert!(COUNTED_LEN.is_multiple_of(COUNTED_CHUNK_LEN));

/// Per sampled position, `1` while every probe chosen so far matches.
type SampleHits = [u8; COUNTED_LEN];

/// Compares one batch of blocks; see [`compare_batch`].
type Compare = fn(&LiteralPlan, &[u8], usize, usize, &mut Hits);

/// A block scanner with its plan for one haystack, or `None` if the scanner is not used.
pub(crate) type BlockPlan<'s> = Option<(&'s LiteralScanner, LiteralPlan)>;

/// Leftmost-longest scanner for the occurrences of up to [`MAX_PATTERNS`] literal patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiteralScanner {
    /// Longest first, then by caller index, so the first literal verified at a start is the
    /// leftmost-longest match.
    literals: Vec<Literal>,
    /// Each literal's index in the caller's pattern list.
    indexes: Vec<usize>,
    insensitive: bool,
}

/// The probes chosen for one haystack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiteralPlan {
    /// Probed offsets, [`MAX_PROBES`] slots per pattern in the scanner's order.
    offsets: [usize; MAX_PATTERNS * MAX_PROBES],
    /// Probed bytes as the kernel compares them, with bit 5 set when folding.
    bytes: [u8; MAX_PATTERNS * MAX_PROBES],
    /// Probes per pattern: one, two or four.
    probes: usize,
    /// Whether haystack letters are compared in either case.
    fold: bool,
    sampled_occurrences: usize,
    sampled_positions: usize,
    sampled_newlines: usize,
    sampled_len: usize,
}

impl LiteralPlan {
    /// Estimates the occurrences in `haystack_len` bytes from the sample's density, plus a
    /// sixteenth of slack, capped at [`EXPECTED_CEILING`].
    pub(crate) fn expected_occurrences(&self, haystack_len: usize) -> usize {
        Self::scaled(
            self.sampled_occurrences,
            self.sampled_positions,
            haystack_len,
        )
    }

    /// Estimates the lines with an occurrence in `haystack_len` bytes: at most the estimated
    /// occurrences and lines.
    pub(crate) fn expected_lines(&self, haystack_len: usize) -> usize {
        // The last line may end without a line feed.
        let lines = Self::scaled(self.sampled_newlines, self.sampled_len, haystack_len) + 1;
        self.expected_occurrences(haystack_len).min(lines)
    }

    /// Scales a count found in `sampled` bytes to `haystack_len` bytes.
    fn scaled(count: usize, sampled: usize, haystack_len: usize) -> usize {
        // Widened so a long haystack times a dense sample cannot overflow.
        let expected = count as u128 * haystack_len as u128 / sampled as u128;
        usize::try_from(expected + expected / 16)
            .map_or(EXPECTED_CEILING, |expected| expected.min(EXPECTED_CEILING))
    }

    /// Whether the sample held an occurrence at least every `spacing` bytes.
    pub(crate) fn is_denser_than(&self, spacing: usize) -> bool {
        self.sampled_occurrences.saturating_mul(spacing) >= self.sampled_positions
    }
}

/// The compiled compare loop for `patterns` patterns of `probes` probes each.
fn compare_for(patterns: usize, probes: usize, fold: bool) -> Compare {
    /// Picks one monomorphized copy.
    fn copy<const PATTERNS: usize, const PROBES: usize>(fold: bool) -> Compare {
        if fold {
            compare_batch::<PATTERNS, PROBES, true>
        } else {
            compare_batch::<PATTERNS, PROBES, false>
        }
    }
    match (patterns, probes) {
        (1, 1) => copy::<1, 1>(fold),
        (2, 1) => copy::<2, 1>(fold),
        (3, 1) => copy::<3, 1>(fold),
        (4, 1) => copy::<4, 1>(fold),
        (5, 1) => copy::<5, 1>(fold),
        (6, 1) => copy::<6, 1>(fold),
        (7, 1) => copy::<7, 1>(fold),
        (8, 1) => copy::<8, 1>(fold),
        (1, 2) => copy::<1, 2>(fold),
        (2, 2) => copy::<2, 2>(fold),
        (3, 2) => copy::<3, 2>(fold),
        (4, 2) => copy::<4, 2>(fold),
        (5, 2) => copy::<5, 2>(fold),
        (6, 2) => copy::<6, 2>(fold),
        (7, 2) => copy::<7, 2>(fold),
        (8, 2) => copy::<8, 2>(fold),
        (1, 4) => copy::<1, 4>(fold),
        (2, 4) => copy::<2, 4>(fold),
        (3, 4) => copy::<3, 4>(fold),
        (4, 4) => copy::<4, 4>(fold),
        (5, 4) => copy::<5, 4>(fold),
        (6, 4) => copy::<6, 4>(fold),
        (7, 4) => copy::<7, 4>(fold),
        (8, 4) => copy::<8, 4>(fold),
        _ => unreachable!("plans hold up to MAX_PATTERNS patterns of one, two or four probes"),
    }
}

/// Flags the starts in `blocks` blocks from `position` where every probe of some pattern
/// matches, recording flagged blocks (numbered from the batch start) in `hits`.
///
/// The const parameters fix every loop bound, so each copy is branch-free straight-line
/// vector code. With `FOLD`, bit 5 of every haystack byte is set before comparing. Every
/// probe of every start in the batch must lie inside `haystack`.
#[inline(never)]
fn compare_batch<const PATTERNS: usize, const PROBES: usize, const FOLD: bool>(
    plan: &LiteralPlan,
    haystack: &[u8],
    position: usize,
    blocks: usize,
    hits: &mut Hits,
) {
    // One stream of blocks per probe, starting at the probe's offset.
    let streams: [[&[[u8; BLOCK]]; PROBES]; PATTERNS] = std::array::from_fn(|pattern| {
        std::array::from_fn(|probe| {
            let offset = plan.offsets[pattern * MAX_PROBES + probe];
            let stream = haystack.get(position + offset..).unwrap_or_default();
            stream.as_chunks().0
        })
    });
    let bytes: [[u8; PROBES]; PATTERNS] = std::array::from_fn(|pattern| {
        std::array::from_fn(|probe| plan.bytes[pattern * MAX_PROBES + probe])
    });
    let mut count = 0_usize;
    for block in 0..blocks {
        let mut flags = [0_u8; BLOCK];
        for (pattern_streams, pattern_bytes) in streams.iter().zip(&bytes) {
            let mut hit = [u8::MAX; BLOCK];
            for (stream, &probed) in pattern_streams.iter().zip(pattern_bytes) {
                for (flag, &byte) in hit.iter_mut().zip(&stream[block]) {
                    let byte = if FOLD { byte | 0x20 } else { byte };
                    *flag &= if byte == probed { u8::MAX } else { 0 };
                }
            }
            for (flag, &hit) in flags.iter_mut().zip(&hit) {
                *flag |= hit;
            }
        }
        let any = flags.iter().fold(0, |any, &flag| any | flag);
        count = hits.record(count, block as u8, flags, any);
    }
    hits.count = count;
}

/// Counts the flagged sampled positions that hold `byte` at `offset` after them.
fn probe_hits(sample: &[SampleWindow], hits: &[SampleHits], offset: usize, byte: u8) -> usize {
    sample
        .iter()
        .zip(hits)
        .map(|(window, hits)| {
            let probed = &window[offset..][..COUNTED_LEN];
            // A chunk's count fits a byte, so it is summed in byte-wide vector lanes.
            let (probed, _) = probed.as_chunks::<COUNTED_CHUNK_LEN>();
            let (hits, _) = hits.as_chunks::<COUNTED_CHUNK_LEN>();
            hits.iter()
                .zip(probed)
                .map(|(hits, probed)| {
                    let count: u8 = hits
                        .iter()
                        .zip(probed)
                        // `u8::from` turns `true` into 1 and `false` into 0.
                        .map(|(&hit, &probed)| hit & u8::from(probed == byte))
                        .sum();
                    usize::from(count)
                })
                .sum::<usize>()
        })
        .sum()
}

/// Clears the flags of sampled positions that do not hold `byte` at `offset` after them.
fn narrow_hits(sample: &[SampleWindow], hits: &mut [SampleHits], offset: usize, byte: u8) {
    for (window, hits) in sample.iter().zip(hits) {
        for (hit, &probed) in hits.iter_mut().zip(&window[offset..]) {
            *hit &= u8::from(probed == byte);
        }
    }
}

/// Counts the positions flagged in `hits` where `head` lies in the sample.
fn sampled_occurrences(sample: &[SampleWindow], hits: &[SampleHits], head: &[u8]) -> usize {
    sample
        .iter()
        .zip(hits)
        .map(|(window, hits)| {
            let mut occurrences = 0_usize;
            // Few positions are still flagged, so skip eight at a time.
            for (word_index, word) in hits.as_chunks::<8>().0.iter().enumerate() {
                if u64::from_ne_bytes(*word) == 0 {
                    continue;
                }
                for (index, &hit) in word.iter().enumerate() {
                    let start = word_index * 8 + index;
                    occurrences += usize::from(hit != 0 && window[start..].starts_with(head));
                }
            }
            occurrences
        })
        .sum()
}

impl LiteralScanner {
    /// Builds a scanner for one to [`MAX_PATTERNS`] non-empty patterns of at most
    /// [`MAX_PATTERN_LEN`] bytes, or `None` otherwise.
    pub(crate) fn new(patterns: &[Vec<u8>], case_mode: CaseMode) -> Option<Self> {
        let scannable = |pattern: &Vec<u8>| (1..=MAX_PATTERN_LEN).contains(&pattern.len());
        if patterns.is_empty() || patterns.len() > MAX_PATTERNS || !patterns.iter().all(scannable) {
            return None;
        }
        let mut order = (0..patterns.len()).collect::<Vec<_>>();
        order.sort_by_key(|&index| (Reverse(patterns[index].len()), index));
        Some(Self {
            literals: order
                .iter()
                .map(|&index| Literal::new(&patterns[index], case_mode))
                .collect(),
            indexes: order,
            insensitive: case_mode == CaseMode::AsciiInsensitive,
        })
    }

    /// Greedily chooses up to [`MAX_PROBES`] offsets of `head` into `offsets`, each the rare
    /// byte leaving the fewest sampled candidates, while another probe repays its compares.
    ///
    /// Returns the number of probes and the sampled occurrences of `head`.
    fn choose_probes(
        head: &[u8],
        sample: &[SampleWindow],
        byte_counts: &[u16; 256],
        offsets: &mut [usize; MAX_PROBES],
    ) -> (usize, usize) {
        let positions = sample.len() * COUNTED_LEN;
        let rarity = |offset: usize| byte_counts[usize::from(head[offset])];
        let rare: [usize; RARE_OFFSETS] = rare_offsets(head, byte_counts);
        let mut all_hits = [[1_u8; COUNTED_LEN]; MAX_SAMPLE_WINDOWS];
        let hits = &mut all_hits[..sample.len()];
        offsets[0] = rare[0];
        narrow_hits(sample, hits, rare[0], head[rare[0]]);
        let mut candidates = usize::from(rarity(rare[0]));
        let mut chosen = 1;
        while chosen < MAX_PROBES.min(head.len()) {
            // Too few candidates left for any probe to repay itself.
            if candidates * PROBE_REPAY_SPACING <= positions {
                break;
            }
            let Some((remaining, offset)) = rare
                .iter()
                .copied()
                .filter(|offset| *offset < head.len() && !offsets[..chosen].contains(offset))
                .map(|offset| (probe_hits(sample, hits, offset, head[offset]), offset))
                .min()
            else {
                break;
            };
            if candidates.saturating_sub(remaining) * PROBE_REPAY_SPACING <= positions {
                break;
            }
            narrow_hits(sample, hits, offset, head[offset]);
            offsets[chosen] = offset;
            chosen += 1;
            candidates = remaining;
        }
        (chosen, sampled_occurrences(sample, hits, head))
    }

    /// Chooses the probes for a haystack, or returns `None` if it is too short to repay
    /// sampling.
    pub(crate) fn plan(&self, haystack: &[u8]) -> Option<LiteralPlan> {
        if haystack.len() < MIN_HAYSTACK_LEN {
            return None;
        }
        let sample = PairSample::new(haystack, self.insensitive)?;
        let sample = sample.windows();
        let window_count = sample.len();
        let byte_counts = count_bytes(sample);

        let mut plan = LiteralPlan {
            offsets: [0; MAX_PATTERNS * MAX_PROBES],
            bytes: [0; MAX_PATTERNS * MAX_PROBES],
            probes: 1,
            fold: false,
            sampled_occurrences: 0,
            sampled_positions: window_count * COUNTED_LEN,
            sampled_newlines: usize::from(byte_counts[usize::from(b'\n')]),
            sampled_len: window_count * SAMPLE_WINDOW_LEN,
        };
        let mut chosen = [0_usize; MAX_PATTERNS];
        for (slot, literal) in self.literals.iter().enumerate() {
            let head = &literal.bytes[..literal.bytes.len().min(HEAD_LEN)];
            let offsets: &mut [usize; MAX_PROBES] = plan.offsets[slot * MAX_PROBES..]
                .first_chunk_mut()
                .expect("every pattern has its MAX_PROBES offsets");
            let (probes, occurrences) = Self::choose_probes(head, sample, &byte_counts, offsets);
            chosen[slot] = probes;
            plan.probes = plan.probes.max(probes);
            plan.sampled_occurrences += occurrences;
        }
        // Compare loops exist only for one, two or four probes.
        if plan.probes == 3 {
            plan.probes = MAX_PROBES;
        }
        for (slot, literal) in self.literals.iter().enumerate() {
            let offsets = &mut plan.offsets[slot * MAX_PROBES..][..MAX_PROBES];
            // A pattern with fewer probes of its own repeats them.
            for probe in chosen[slot]..plan.probes {
                offsets[probe] = offsets[probe % chosen[slot]];
            }
            for (byte, &offset) in plan.bytes[slot * MAX_PROBES..].iter_mut().zip(&*offsets) {
                *byte = literal.bytes[offset];
            }
        }
        // Insensitive patterns are lowercase, so fold only if some probe is a letter. Folding
        // sets bit 5 on both sides; the extra non-letter candidates it admits fail verification.
        plan.fold = self.insensitive && plan.bytes.iter().any(u8::is_ascii_lowercase);
        if plan.fold {
            plan.bytes.iter_mut().for_each(|byte| *byte |= 0x20);
        }
        Some(plan)
    }

    /// Bytes compared per haystack position under `plan`.
    pub(crate) fn compares_per_position(&self, plan: &LiteralPlan) -> usize {
        self.literals.len() * plan.probes
    }

    /// The leftmost-longest match at `start`, if any. `near_end` adds bounds checks for when
    /// fewer than [`HEAD_LEN`] bytes may follow `start`.
    #[inline(always)]
    fn match_at(&self, haystack: &[u8], start: usize, near_end: bool) -> Option<Match> {
        for (literal, &pattern_index) in self.literals.iter().zip(&self.indexes) {
            let occurs = if near_end {
                haystack
                    .get(start..start + literal.bytes.len())
                    .is_some_and(|window| {
                        if self.insensitive {
                            window_matches_ascii_insensitive(window, &literal.bytes)
                        } else {
                            window == literal.bytes
                        }
                    })
            } else {
                literal.occurs_at(haystack, start, self.insensitive)
            };
            if occurs {
                return Some(Match {
                    start,
                    end: start + literal.bytes.len(),
                    pattern_index,
                });
            }
        }
        None
    }

    /// Visits leftmost-longest occurrences in start order.
    ///
    /// `visit` returns the next start it accepts: the match end, `start + 1` for overlaps, or
    /// [`STOP`]. Returns `None` once the haystack is done or stopped. If candidates keep
    /// failing verification (the sample misjudged the haystack), it gives up and returns the
    /// offset from which the caller must continue.
    #[inline(always)]
    pub(crate) fn scan(
        &self,
        plan: &LiteralPlan,
        haystack: &[u8],
        mut visit: impl FnMut(Match) -> usize,
    ) -> Option<usize> {
        let pattern_count = self.literals.len();
        // Whole blocks of starts that each have a full head after them for probes and
        // verification.
        let scannable = haystack.len().saturating_sub(HEAD_LEN) / BLOCK * BLOCK;
        let compare = compare_for(pattern_count, plan.probes, plan.fold);
        let mut hits = Hits::empty();
        let mut rejected = 0_usize;
        let mut next_start = 0_usize;
        let mut position = 0_usize;
        while position < scannable {
            let blocks = ((scannable - position) / BLOCK).min(BATCH);
            compare(plan, haystack, position, blocks, &mut hits);
            let progress = (&mut next_start, &mut rejected);
            if !self.settle::<false>(haystack, &hits, position, progress, &mut visit) {
                return None;
            }
            position += blocks * BLOCK;
            if rejected > REJECTED_ALLOWANCE + position / REJECTED_SPACING {
                return Some(position.max(next_start).min(haystack.len()));
            }
        }
        // The tail is compared in a zero-padded copy; padding can only add candidates, which
        // verification against the real haystack rejects.
        let tail = &haystack[position..];
        let mut padded = [0_u8; TAIL_BLOCKS * BLOCK + HEAD_LEN];
        padded[..tail.len()].copy_from_slice(tail);
        compare(plan, &padded, 0, tail.len().div_ceil(BLOCK), &mut hits);
        let progress = (&mut next_start, &mut rejected);
        self.settle::<true>(haystack, &hits, position, progress, &mut visit);
        None
    }

    /// Verifies one batch's candidates and visits the occurrences, updating the next
    /// accepted start and the rejected count. Returns `false` once `visit` returns [`STOP`].
    ///
    /// Kept out of line so it does not compete with the compare loops for registers. With
    /// `NEAR_END`, candidates are bounds-checked since they may lie near or past the end.
    #[inline(never)]
    fn settle<const NEAR_END: bool>(
        &self,
        haystack: &[u8],
        hits: &Hits,
        batch_start: usize,
        (next_start, rejected): (&mut usize, &mut usize),
        visit: &mut impl FnMut(Match) -> usize,
    ) -> bool {
        for (block, mut candidates) in hits.flagged() {
            let block_start = batch_start + block * BLOCK;
            while candidates != 0 {
                let start = block_start + candidates.trailing_zeros() as usize;
                // Clears the lowest set bit.
                candidates &= candidates - 1;
                if start < *next_start {
                    continue;
                }
                if let Some(matched) = self.match_at(haystack, start, NEAR_END) {
                    *next_start = visit(matched);
                    if *next_start == STOP {
                        return false;
                    }
                } else {
                    *rejected += 1;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Leftmost-longest matches by trying every pattern at every start.
    fn reference(
        patterns: &[Vec<u8>],
        haystack: &[u8],
        insensitive: bool,
        overlapping: bool,
    ) -> Vec<Match> {
        let mut matches = Vec::new();
        let mut start = 0;
        while start < haystack.len() {
            let best = patterns
                .iter()
                .enumerate()
                .filter(|(_, pattern)| {
                    haystack
                        .get(start..start + pattern.len())
                        .is_some_and(|window| {
                            if insensitive {
                                window.eq_ignore_ascii_case(pattern)
                            } else {
                                window == pattern.as_slice()
                            }
                        })
                })
                .min_by_key(|(index, pattern)| (Reverse(pattern.len()), *index));
            match best {
                Some((pattern_index, pattern)) => {
                    let end = start + pattern.len();
                    matches.push(Match {
                        start,
                        end,
                        pattern_index,
                    });
                    start = if overlapping { start + 1 } else { end };
                }
                None => start += 1,
            }
        }
        matches
    }

    /// Scans `haystack` with a plan made for `planned_on`, finishing with the reference
    /// search if the scan gives up.
    fn scanned(
        patterns: &[Vec<u8>],
        (planned_on, haystack): (&[u8], &[u8]),
        case_mode: CaseMode,
        overlapping: bool,
    ) -> Vec<Match> {
        let scanner = LiteralScanner::new(patterns, case_mode).expect("pattern sets are small");
        let plan = scanner.plan(planned_on).expect("long enough to sample");
        let mut matches = Vec::new();
        let gave_up = scanner.scan(&plan, haystack, |matched| {
            matches.push(matched);
            if overlapping {
                matched.start + 1
            } else {
                matched.end
            }
        });
        if let Some(resume) = gave_up {
            let rest = reference(
                patterns,
                &haystack[resume..],
                scanner.insensitive,
                overlapping,
            );
            matches.extend(rest.into_iter().map(|matched| Match {
                start: matched.start + resume,
                end: matched.end + resume,
                pattern_index: matched.pattern_index,
            }));
        }
        matches
    }

    /// A deterministic text over `alphabet` with every pattern planted at varying gaps.
    fn corpus(alphabet: &[u8], patterns: &[Vec<u8>], len: usize) -> Vec<u8> {
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut text = Vec::with_capacity(len + 64);
        while text.len() < len {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let draw = (seed >> 33) as usize;
            if draw.is_multiple_of(23) {
                text.extend_from_slice(&patterns[draw / 23 % patterns.len()]);
            } else {
                text.push(alphabet[draw % alphabet.len()]);
            }
        }
        text.truncate(len);
        text
    }

    fn pattern_set(patterns: &[&str]) -> Vec<Vec<u8>> {
        patterns
            .iter()
            .map(|pattern| pattern.as_bytes().to_vec())
            .collect()
    }

    /// Uppercases every third byte.
    fn mixed_case(text: &[u8]) -> Vec<u8> {
        text.iter()
            .enumerate()
            .map(|(index, byte)| {
                if index % 3 == 0 {
                    byte.to_ascii_uppercase()
                } else {
                    *byte
                }
            })
            .collect()
    }

    #[test]
    fn scans_match_the_reference_across_lengths_and_modes() {
        let sets = [
            pattern_set(&["error"]),
            pattern_set(&["ab", "abc", "bca", "c"]),
            pattern_set(&["status=500", "status", "warn", "war", "x"]),
            pattern_set(&["aa", "aaa", "aa"]),
            pattern_set(&["a\nb", "\n", "0123456789abcdef0123456789abcdef0123"]),
            // Far longer than a head, so probes and the first compare cover only its start.
            pattern_set(&[&"abcestuwrn".repeat(6), "a\0b", "\0\0"]),
            pattern_set(&["a1", "b2", "c3", "e4", "s5", "t6", "u7", "w8"]),
        ];
        // Lengths around the block size exercise the tail and the first whole block.
        let lengths = [
            0, 1, 5, 63, 64, 65, 79, 80, 81, 127, 128, 129, 200, 1_000, 5_000,
        ];
        for patterns in &sets {
            let lower = corpus(b"abcestuwrnx=05 \n", patterns, MIN_HAYSTACK_LEN + 5_000);
            let mixed = mixed_case(&lower);
            for len in lengths.into_iter().chain([lower.len()]) {
                for overlapping in [false, true] {
                    assert_eq!(
                        scanned(
                            patterns,
                            (&lower, &lower[..len]),
                            CaseMode::Sensitive,
                            overlapping
                        ),
                        reference(patterns, &lower[..len], false, overlapping),
                        "sensitive {len} {overlapping}"
                    );
                    assert_eq!(
                        scanned(
                            patterns,
                            (&mixed, &mixed[..len]),
                            CaseMode::AsciiInsensitive,
                            overlapping
                        ),
                        reference(patterns, &mixed[..len], true, overlapping),
                        "insensitive {len} {overlapping}"
                    );
                }
            }
        }
    }

    #[test]
    fn small_alphabets_get_four_probes_and_keep_every_match() {
        let patterns = pattern_set(&["ACGTAC", "TTTTT", "GGGG"]);
        let haystack = corpus(b"ACGT", &patterns, 50_000);
        let scanner = LiteralScanner::new(&patterns, CaseMode::Sensitive).expect("three patterns");
        let plan = scanner.plan(&haystack).expect("long enough");

        let matches = scanned(
            &patterns,
            (&haystack, &haystack),
            CaseMode::Sensitive,
            false,
        );

        assert_eq!(plan.probes, MAX_PROBES);
        assert!(matches.len() > 100);
        assert_eq!(matches, reference(&patterns, &haystack, false, false));
    }

    #[test]
    fn plans_probe_rare_bytes_once_and_fold_only_letters() {
        let lines = (0..2_000)
            .map(|index| format!("{index:05} INFO request_id={index} [k_v] done\n"))
            .collect::<String>();
        let sparse = pattern_set(&["request_id="]);
        let scanner = LiteralScanner::new(&sparse, CaseMode::Sensitive).expect("one pattern");
        let plan = scanner.plan(lines.as_bytes()).expect("long enough");
        // One byte that only this word of a line holds leaves a candidate per line.
        assert_eq!(plan.probes, 1);
        assert!(b"qrusi=".contains(&plan.bytes[0]), "{}", plan.bytes[0]);
        assert!(!plan.fold);

        let folded = LiteralScanner::new(&pattern_set(&["info"]), CaseMode::AsciiInsensitive)
            .expect("one pattern");
        let plan = folded.plan(lines.as_bytes()).expect("long enough");
        assert!(plan.fold);
        assert!(
            plan.bytes[..plan.probes]
                .iter()
                .all(|byte| byte & 0x20 != 0)
        );

        let digits = LiteralScanner::new(&pattern_set(&["=1"]), CaseMode::AsciiInsensitive)
            .expect("one pattern");
        assert!(!digits.plan(lines.as_bytes()).expect("long enough").fold);

        assert!(
            scanner
                .plan(&lines.as_bytes()[..MIN_HAYSTACK_LEN - 1])
                .is_none()
        );
        let too_many = vec![b"a".to_vec(); MAX_PATTERNS + 1];
        let too_long = vec![vec![b'a'; MAX_PATTERN_LEN + 1]];
        assert!(LiteralScanner::new(&[], CaseMode::Sensitive).is_none());
        assert!(LiteralScanner::new(&[Vec::new()], CaseMode::Sensitive).is_none());
        assert!(LiteralScanner::new(&too_many, CaseMode::Sensitive).is_none());
        assert!(LiteralScanner::new(&too_long, CaseMode::Sensitive).is_none());
    }

    #[test]
    fn expected_occurrences_follow_the_sampled_density() {
        let dense = b"needle warning info ".repeat(2_000);
        let scanner = LiteralScanner::new(&pattern_set(&["needle"]), CaseMode::Sensitive)
            .expect("one pattern");
        let plan = scanner.plan(&dense).expect("long enough");
        let expected = plan.expected_occurrences(dense.len());
        assert!((2_000..2_400).contains(&expected), "{expected}");
        assert!(plan.is_denser_than(32));
        assert!(!plan.is_denser_than(8));

        let absent = LiteralScanner::new(&pattern_set(&["zebra"]), CaseMode::Sensitive)
            .expect("one pattern");
        let plan = absent.plan(&dense).expect("long enough");
        assert_eq!(plan.expected_occurrences(dense.len()), 0);
        assert_eq!(plan.expected_lines(dense.len()), 0);
        assert!(!plan.is_denser_than(1 << 20));

        // Every position starts an occurrence, on one line: matches are capped, lines are one.
        let run = vec![b'a'; 64 * MIN_HAYSTACK_LEN];
        let repeated =
            LiteralScanner::new(&pattern_set(&["aa"]), CaseMode::Sensitive).expect("one pattern");
        let plan = repeated.plan(&run).expect("long enough");
        assert_eq!(plan.expected_occurrences(run.len()), EXPECTED_CEILING);
        assert_eq!(plan.expected_lines(run.len()), 1);
    }

    #[test]
    fn visitors_can_stop_and_skip_ahead() {
        let patterns = pattern_set(&["ab"]);
        let haystack = b"ab ab ab\nab ab\nab".repeat(2_000);
        let scanner = LiteralScanner::new(&patterns, CaseMode::Sensitive).expect("one pattern");
        let plan = scanner.plan(&haystack).expect("long enough");

        let mut first_three = Vec::new();
        let stopped = scanner.scan(&plan, &haystack, |matched| {
            first_three.push(matched.start);
            if first_three.len() == 3 {
                STOP
            } else {
                matched.end
            }
        });
        let mut line_firsts = 0;
        let finished = scanner.scan(&plan, &haystack, |matched| {
            line_firsts += 1;
            haystack[matched.end..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(haystack.len(), |offset| matched.end + offset + 1)
        });

        assert_eq!((stopped, finished), (None, None));
        assert_eq!(first_three, [0, 3, 6]);
        assert_eq!(line_firsts, haystack.split(|&byte| byte == b'\n').count());
    }

    #[test]
    fn scans_give_up_when_the_sample_misjudged_the_haystack() {
        let patterns = pattern_set(&["needle"]);
        let scanner = LiteralScanner::new(&patterns, CaseMode::Sensitive).expect("one pattern");
        // Planned on text without the probed byte, then run on text that is all decoys.
        let quiet = vec![b'.'; MIN_HAYSTACK_LEN];
        let plan = scanner.plan(&quiet).expect("long enough");
        let mut decoys = vec![b'.'; 4 * MIN_HAYSTACK_LEN];
        for index in (0..decoys.len()).step_by(8) {
            decoys[index] = plan.bytes[0];
        }
        let tail = decoys.len() - 6;
        decoys[tail..].copy_from_slice(b"needle");

        let gave_up = scanner.scan(&plan, &decoys, |matched| matched.end);

        assert!(gave_up.is_some_and(|resume| resume <= 2 * BATCH * BLOCK));
        assert_eq!(
            scanned(&patterns, (&quiet, &decoys), CaseMode::Sensitive, false),
            [Match {
                start: tail,
                end: decoys.len(),
                pattern_index: 0
            }]
        );
    }
}
