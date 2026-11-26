//! Choosing the pairs a [`PairScanner`] scans for, from a sample of the haystack.
//!
//! A small set uses each pattern's rarest pair whenever the sample holds them seldom. A
//! larger set, or non-rarest pairs, is scanned only if a cost model (the constants below)
//! predicts the scan beats packed search. That weighing happens only on targets the model
//! fits and for inputs long enough to repay it ([`Extent::weighs_scans`]).

use std::cell::LazyCell;
use std::num::NonZeroUsize;

use memchr::{memchr, memchr_iter, memmem};
use smallvec::SmallVec;

use super::{
    COUNTED_LEN, MAX_DISTANCE, MAX_PATTERNS, MIN_CANDIDATE_SPACING, MIN_HAYSTACK_LEN, PairEnd,
    PairPlan, PairSample, PairScanner, RARE_OFFSETS, RIVAL_RARE_SPACING, SAMPLE_WINDOW_LEN,
    SampleWindow, WEIGHS_SCANS, count_bytes, rare_offsets, sampled_occurrences,
};

/// Largest pattern set whose rarest pairs are scanned without weighing against packed search.
const MAX_UNWEIGHED_PATTERNS: usize = 4;
/// Positions that one vector operation covers where scans are weighed.
const VECTOR_LEN: usize = 16;
/// Estimated cost of packed search per [`VECTOR_LEN`] bytes searched, in compare-loop
/// vector operations.
const PACKED_VECTOR_COST: usize = 17;
/// [`PACKED_VECTOR_COST`] for case-insensitive patterns, which also fold the haystack.
const PACKED_FOLDED_VECTOR_COST: usize = 23;
/// Estimated cost per [`VECTOR_LEN`] bytes packed search skips to a matched line's end.
const PACKED_SKIP_VECTOR_COST: usize = 2;
/// Estimated cost of one packed search stop: verify, find the line end, restart.
const PACKED_STOP_COST: usize = 350;
/// Estimated cost of one scan candidate: locating it, a mispredicted branch, verification.
const CANDIDATE_COST: usize = 128;
/// A weighed scan must save one part in this many of packed search's estimated cost, since
/// closer estimates cannot tell which is faster.
const MIN_GAIN_SHARE: usize = 8;
/// Fewest bytes one thread must search for weighing to repay its cost.
pub(super) const MIN_WEIGHED_SHARE_LEN: usize = 1024 * 1024;

/// How the haystack given to [`PairScanner::plan`] relates to the whole input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Extent {
    /// The whole input.
    Whole {
        /// Threads searching equal shares of it. `NonZeroUsize` makes division infallible.
        threads: NonZeroUsize,
    },
    /// One of the pieces the input arrives in, searched in order by one thread.
    Piece {
        /// Bytes of the input before the piece.
        preceded_by: usize,
    },
}

impl Extent {
    /// A whole input searched by one thread.
    pub(crate) const ON_ONE_THREAD: Self = Self::Whole {
        threads: NonZeroUsize::MIN,
    };

    /// Whether scans of a `len`-byte haystack are weighed against packed search: only on
    /// targets the cost model fits, and when one thread searches at least
    /// [`MIN_WEIGHED_SHARE_LEN`] bytes.
    pub(crate) fn weighs_scans(self, len: usize) -> bool {
        WEIGHS_SCANS && self.share(len) >= MIN_WEIGHED_SHARE_LEN
    }

    /// Bytes one thread is known to search: its share of a whole input, or everything up to
    /// the end of this piece.
    fn share(self, len: usize) -> usize {
        match self {
            Self::Whole { threads } => len / threads,
            Self::Piece { preceded_by } => preceded_by.saturating_add(len),
        }
    }
}

/// One pair of a pattern as ranked by [`PairScanner::ranked_pairs`]. The derived order
/// compares fields top to bottom, so the smallest is the rarest pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct RankedPair {
    /// Sampled occurrences of the pair.
    pub(super) occurrences: usize,
    /// Summed sample counts of the pair's two bytes, breaking ties toward rarer bytes.
    pub(super) byte_occurrences: u16,
    /// Offset of the pair's first byte in the pattern.
    pub(super) offset: usize,
}
/// One pattern's ranked pairs at one distance, at most two per rare offset.
pub(super) type RankedPairs = SmallVec<[RankedPair; 2 * RARE_OFFSETS]>;

/// What packed search does when counting a sample's lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PackedWork {
    /// Matching lines it stops at.
    pub(super) stops: usize,
    /// Bytes skipped from each line's first match to its end (estimated past a window's
    /// end), at most the sample's length.
    pub(super) skipped: usize,
}

/// [`PairScanner::affordable_candidates`] bound to one sample. `&dyn Fn` is a reference to
/// any closure with that signature.
pub(super) type Affordable<'a> = &'a dyn Fn(usize) -> Option<usize>;

// Compile-time check: unweighed sets fit within the scanner's limit.
const _: () = assert!(MAX_UNWEIGHED_PATTERNS <= MAX_PATTERNS);

impl PairPlan {
    /// Compare-loop vector operations per [`VECTOR_LEN`] positions: a compare and select per
    /// pair, plus a value compare and a flag accumulation, plus two more to `fold`.
    pub(super) fn compare_operations(pair_count: usize, fold: bool) -> usize {
        2 * pair_count + 2 + if fold { 2 } else { 0 }
    }
}

impl PairScanner {
    /// Ranks, rarest first, the pairs of `pattern` at `distance` that include one of its
    /// `rare` offsets. Empty if the pattern is too short.
    pub(super) fn ranked_pairs(
        pattern: &[u8],
        rare: &[usize; RARE_OFFSETS],
        distance: usize,
        sample: &[SampleWindow],
        byte_counts: &[u16; 256],
    ) -> RankedPairs {
        let rarity = |offset: usize| byte_counts[usize::from(pattern[offset])];
        let mut ranked: RankedPairs = (0..pattern.len().saturating_sub(distance))
            .filter(|&offset| rare.contains(&offset) || rare.contains(&(offset + distance)))
            .map(|offset| RankedPair {
                occurrences: sampled_occurrences(
                    sample,
                    distance,
                    [pattern[offset], pattern[offset + distance]],
                ),
                byte_occurrences: rarity(offset) + rarity(offset + distance),
                offset,
            })
            .collect();
        ranked.sort_unstable();
        ranked
    }

    /// Greedily chooses one pair per pattern from `ranked`, in pattern order.
    ///
    /// With `distinct_end`, each pair must equal or differ at that end from every earlier
    /// choice; with `None`, each pattern takes its rarest pair. Returns each pattern's pair
    /// offset and the pairs' total sampled occurrences, or `None` if a pattern runs out.
    pub(super) fn choose_pairs(
        &self,
        distance: usize,
        ranked: &[RankedPairs],
        distinct_end: Option<PairEnd>,
    ) -> Option<([usize; MAX_PATTERNS], usize)> {
        // Compare as if folding, so a pair that only differs in bit 5 is conservatively
        // treated as equal.
        let fold_bit = if self.insensitive { 0x20 } else { 0 };
        let mut anchors = [0_usize; MAX_PATTERNS];
        let mut chosen: SmallVec<[[u8; 2]; MAX_PATTERNS]> = SmallVec::new();
        let mut candidates = 0_usize;
        for ((pattern, ranked), anchor) in self.patterns.iter().zip(ranked).zip(&mut anchors) {
            let pair_at = |offset: usize| {
                [pattern.bytes[offset], pattern.bytes[offset + distance]]
                    .map(|byte| byte | fold_bit)
            };
            let choice = ranked.iter().find(|candidate| {
                let pair = pair_at(candidate.offset);
                distinct_end.is_none_or(|end| {
                    chosen
                        .iter()
                        .all(|other| *other == pair || other[end.index()] != pair[end.index()])
                })
            })?;
            *anchor = choice.offset;
            chosen.push(pair_at(choice.offset));
            candidates += choice.occurrences;
        }
        Some((anchors, candidates))
    }

    /// The plan at `distance` and its sampled candidate count, if a scan is worth it.
    ///
    /// Uses every pattern's rarest pair if their keys can be distinct; otherwise, when
    /// weighing (`affordable` is `Some`), picks per-pattern pairs distinct at whichever end
    /// yields fewer candidates. Only the rarest pairs of at most [`MAX_UNWEIGHED_PATTERNS`]
    /// patterns skip weighing. `absent` is the rarest byte and its sample count.
    pub(super) fn plan_at(
        &self,
        distance: usize,
        ranked: &[RankedPairs],
        absent: (u8, usize),
        affordable: Option<Affordable<'_>>,
    ) -> Option<(usize, PairPlan)> {
        let (rarest, rarest_count) = absent;
        let planned = |distinct_end: Option<PairEnd>| {
            let (anchors, mut candidates) = self.choose_pairs(distance, ranked, distinct_end)?;
            let plan = PairPlan::new(self, distance, anchors, rarest)?;
            if plan.flags_absent() {
                // Slightly overcounted: whole windows, not just counted positions.
                candidates += rarest_count;
            }
            Some((candidates, plan))
        };
        let rarest_pairs = planned(None);
        if rarest_pairs.is_some() && self.patterns.len() <= MAX_UNWEIGHED_PATTERNS {
            return rarest_pairs;
        }
        let affordable = affordable?;
        // Non-rarest pairs are often frequent, so they are weighed like a large set. Ties
        // go to the first end (`min_by_key` keeps the first minimum).
        let (candidates, plan) = rarest_pairs.or_else(|| {
            [PairEnd::First, PairEnd::Second]
                .into_iter()
                .filter_map(|end| planned(Some(end)))
                .min_by_key(|&(candidates, _)| candidates)
        })?;
        let operations = PairPlan::compare_operations(plan.pair_count, plan.fold);
        affordable(operations)
            .is_some_and(|most| candidates <= most)
            .then_some((candidates, plan))
    }

    /// The plan with the fewest sampled candidates over all distances (ties to the shortest
    /// distance), or `None` if none has at most `admitted`.
    pub(super) fn rarest_plan(
        &self,
        sample: &[SampleWindow],
        byte_counts: &[u16; 256],
        admitted: usize,
        affordable: Option<Affordable<'_>>,
    ) -> Option<PairPlan> {
        let rare: SmallVec<[[usize; RARE_OFFSETS]; MAX_PATTERNS]> = self
            .patterns
            .iter()
            .map(|pattern| rare_offsets(&pattern.bytes, byte_counts))
            .collect();
        // Prefer the highest byte on ties: UTF-8 text never holds `0xFF`.
        let rarest = (0..=u8::MAX)
            .rev()
            .min_by_key(|&byte| byte_counts[usize::from(byte)])
            .expect("the range of all bytes is not empty");
        let absent = (rarest, usize::from(byte_counts[usize::from(rarest)]));
        let mut best: Option<(usize, PairPlan)> = None;
        for distance in 1..=MAX_DISTANCE.min(self.shortest - 1) {
            // A later distance must strictly beat the best so far; zero cannot be beaten.
            let limit = match &best {
                Some((0, _)) => break,
                Some((candidates, _)) => candidates - 1,
                None => admitted,
            };
            // The rarest pairs' sum bounds any plan from below, so stop counting once it
            // passes the limit.
            let mut fewest = 0_usize;
            let mut ranked: SmallVec<[RankedPairs; MAX_PATTERNS]> = SmallVec::new();
            for (pattern, rare) in self.patterns.iter().zip(&rare) {
                let pairs = Self::ranked_pairs(&pattern.bytes, rare, distance, sample, byte_counts);
                let Some(least) = pairs.first() else {
                    break;
                };
                fewest += least.occurrences;
                if fewest > limit {
                    break;
                }
                ranked.push(pairs);
            }
            if ranked.len() < self.patterns.len() {
                continue;
            }
            let planned = self
                .plan_at(distance, &ranked, absent, affordable)
                .filter(|&(candidates, _)| candidates <= limit);
            best = planned.or(best);
        }
        best.map(|(_, plan)| plan)
    }

    /// Chooses the pairs for `haystack`, or returns `None` if it is too short, no distance
    /// yields usable pairs, the pairs are too frequent, or another search should win (the
    /// rival byte is rarer, or weighing is required but unavailable or unfavorable).
    pub(crate) fn plan(&self, haystack: &[u8], extent: Extent) -> Option<PairPlan> {
        let weighs = extent.weighs_scans(haystack.len());
        let large = self.patterns.len() > MAX_UNWEIGHED_PATTERNS;
        if haystack.len() < MIN_HAYSTACK_LEN || (large && !weighs) {
            return None;
        }
        // Weighing is costly per window, so a large set's sample scales with one thread's
        // share rather than the whole haystack.
        let sampled_share = if large {
            extent.share(haystack.len()).min(haystack.len())
        } else {
            haystack.len()
        };
        let sample = PairSample::for_share(haystack, self.insensitive, sampled_share)?;
        let sample_len = sample.counted_positions();
        let sample = sample.windows();
        let window_count = sample.len();
        // Packed work is sampled only when weighing needs it.
        let packed = LazyCell::new(|| self.sampled_packed_work(sample));
        let affordable =
            |operations: usize| self.affordable_candidates(operations, *packed, window_count);
        // Checked before the costlier pair counting: bail if even a candidate-free scan of
        // this many pairs loses.
        let affordable_unshared = if large {
            affordable(PairPlan::compare_operations(self.patterns.len(), false))?
        } else {
            usize::MAX
        };
        let rival_candidates = self.rival.map_or(usize::MAX, |rival| {
            sample
                .iter()
                .map(|window| memchr_iter(rival, &window[..COUNTED_LEN]).count())
                .sum()
        });
        // Also checked before the costlier pair counting.
        if rival_candidates.saturating_mul(RIVAL_RARE_SPACING) <= sample_len {
            return None;
        }
        let byte_counts = count_bytes(sample);
        let admitted = (sample_len / MIN_CANDIDATE_SPACING)
            .min(rival_candidates)
            .min(affordable_unshared);
        let affordable = weighs.then_some(&affordable as Affordable<'_>);
        let mut plan = self.rarest_plan(sample, &byte_counts, admitted, affordable)?;
        plan.key_on_rarer_byte(&byte_counts, window_count * SAMPLE_WINDOW_LEN);

        Some(plan)
    }

    fn packed_vector_cost(&self) -> usize {
        if self.insensitive {
            PACKED_FOLDED_VECTOR_COST
        } else {
            PACKED_VECTOR_COST
        }
    }

    /// Simulates packed search counting lines over `sample`: stop at a match, skip to the
    /// line's end.
    ///
    /// A match in a partial line at the start of a later window counts as skipped, not as
    /// a stop. A line running past its window is assumed to be of average length.
    pub(super) fn sampled_packed_work(&self, sample: &[SampleWindow]) -> PackedWork {
        let finders: SmallVec<[memmem::Finder<'_>; MAX_PATTERNS]> = self
            .patterns
            .iter()
            .map(|pattern| memmem::Finder::new(&pattern.bytes))
            .collect();
        // `as_flattened` views the array of windows as one contiguous slice.
        let sampled = sample.as_flattened();
        let line_len = sampled.len() / memchr_iter(b'\n', sampled).count().max(1);
        let mut work = PackedWork {
            stops: 0,
            skipped: 0,
        };
        for (index, window) in sample.iter().enumerate() {
            // Occurrences spanning two windows are missed.
            let mut occurs = [false; SAMPLE_WINDOW_LEN];
            for finder in &finders {
                for start in finder.find_iter(window) {
                    occurs[start] = true;
                }
            }
            // Start of the first whole line; only the first window is known to start one.
            let begun = if index == 0 {
                0
            } else {
                memchr(b'\n', window).map_or(SAMPLE_WINDOW_LEN, |end| end + 1)
            };
            if occurs[..begun].contains(&true) {
                work.skipped += begun;
            }
            let mut searched_from = begun;
            while let Some(found) = occurs[searched_from..].iter().position(|&marked| marked) {
                let start = searched_from + found;
                // Without a line feed in the window, assume the line runs `line_len` more.
                let (line_end, unseen) = memchr(b'\n', &window[start..])
                    .map_or((SAMPLE_WINDOW_LEN, line_len), |end| (start + end + 1, 0));
                work.stops += 1;
                work.skipped += line_end - start + unseen;
                searched_from = line_end;
            }
        }
        work.skipped = work.skipped.min(sampled.len());
        work
    }

    /// Most sampled candidates a scan spending `operations` per vector can meet and still
    /// save one part in [`MIN_GAIN_SHARE`] over packed search, or `None` if it cannot even
    /// with none. Other searches the engine may use are assumed no faster than packed search.
    pub(super) fn affordable_candidates(
        &self,
        operations: usize,
        packed: PackedWork,
        window_count: usize,
    ) -> Option<usize> {
        let searched = (window_count * SAMPLE_WINDOW_LEN).saturating_sub(packed.skipped);
        // All terms are in units of vector operations times bytes.
        let packed_cost = self.packed_vector_cost() * searched
            + PACKED_SKIP_VECTOR_COST * packed.skipped
            + VECTOR_LEN * PACKED_STOP_COST * packed.stops;
        // Scaled to the counted positions, then reduced by the required gain.
        let scan_budget =
            packed_cost * COUNTED_LEN / SAMPLE_WINDOW_LEN * (MIN_GAIN_SHARE - 1) / MIN_GAIN_SHARE;
        let compares = operations * window_count * COUNTED_LEN;
        // `None` if the compares alone use up the budget.
        scan_budget
            .checked_sub(compares + 1)
            .map(|spare| spare / (VECTOR_LEN * CANDIDATE_COST))
    }
}
