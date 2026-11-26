use std::borrow::Cow;
use std::ops::Range;

use memchr::memchr_iter;
use smallvec::SmallVec;
use triple_accel::SearchType;
use triple_accel::levenshtein::{LEVENSHTEIN_COSTS, levenshtein_search_simd_with_opts};

use crate::chunks::{
    concat_chunks, map_chunks, merge_chunks, release_unused_capacity, reserve_for_other_chunks,
};
use crate::exact::PairSample;
use crate::{
    CaseMode, Match, SearchConfig, SearchError, is_word_boundary, normalize_haystack,
    normalize_pattern, profiles,
};

/// One haystack with how it is searched.
struct PrefilterContext<'a> {
    haystack: &'a [u8],
    /// Whether ASCII case is ignored: the patterns are stored lowered, and the passes lower
    /// the haystack bytes they read.
    folds: bool,
    config: SearchConfig,
    line_local: bool,
}

impl<'a> PrefilterContext<'a> {
    /// The context of a search of `haystack` with `config`.
    fn new(haystack: &'a [u8], config: SearchConfig, line_local: bool) -> Self {
        Self {
            haystack,
            folds: ignores_case(config),
            config,
            line_local,
        }
    }
}

/// Whether letters match in either ASCII case under `config`, which the scans of the
/// haystack then allow for themselves rather than reading a lowered copy.
fn ignores_case(config: SearchConfig) -> bool {
    config.case_mode() == CaseMode::AsciiInsensitive
}

/// Approximate (fuzzy) matching engine using Levenshtein edit distance.
///
/// A window within one edit of a pattern contains one of its halves unedited, so the main
/// pass scans for two "mark" bytes of each half and verifies the windows around each hit.
/// Text where marks are dense uses a bit-parallel pass instead, and patterns without halves
/// (too short, or edit distance other than one) fall back to SIMD Levenshtein.
pub(crate) struct ApproximateEngine {
    patterns: Vec<ApproximatePattern>,
    max_edit_distance: u32,
    bitap: Option<Box<BitapPrefilter>>,
    /// Default marks, used for haystacks too short to sample.
    marks: MarkPlan,
    /// Number of candidate byte pairs weighed when choosing marks from a sample.
    mark_choices: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ApproximateCandidate {
    matched: Match,
    edit_distance: u32,
}

impl ApproximateCandidate {
    #[inline(always)]
    fn new(start: usize, end: usize, pattern_index: usize, edit_distance: u32) -> Self {
        Self {
            matched: Match {
                start,
                end,
                pattern_index,
            },
            edit_distance,
        }
    }

    /// Keeps each start's best candidate from a run sorted by `approximate_candidate_cmp`.
    fn dedup_starts(mut candidates: Vec<Self>) -> Vec<Self> {
        candidates.dedup_by_key(|candidate| candidate.matched.start);
        candidates
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApproximatePattern {
    bytes: Vec<u8>,
    /// Packed form for fast window comparison; `None` if the pattern is too long.
    packed: Option<PackedPattern>,
    /// Ranges of the pattern's two halves, if its length and edit distance allow them.
    halves: Option<[Range<usize>; 2]>,
}

impl ApproximatePattern {
    fn new(bytes: Vec<u8>, max_edit_distance: u32) -> Self {
        Self {
            packed: PackedPattern::new(&bytes),
            halves: pattern_halves(&bytes, max_edit_distance),
            bytes,
        }
    }
}

/// Plan for counting matching lines without a full search: every pattern half with its
/// marks, all in a single pass group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApproximateLinePlan {
    halves: Vec<MarkedHalf>,
}

/// Minimum parallel region length for the bit-parallel pass and the mark pass. The mark
/// pass is cheap per byte, so its regions are larger to amortize per-region costs.
const MIN_DENSE_REGION_LEN: usize = 16 * 1024;
const MIN_SPARSE_REGION_LEN: usize = 64 * 1024;
/// Regions per pool thread, so slower threads can take fewer.
const REGIONS_PER_THREAD: usize = 4;
/// Minimum haystack length for parallel search when per-byte work is high (several
/// patterns, or the bit-parallel pass).
const DENSE_PARALLEL_LEN: usize = 64 * 1024;
/// Region length for single-thread pools, where regions only bound candidate list sizes.
const SEQUENTIAL_REGION_LEN: usize = 1024 * 1024;
/// Minimum haystack length sampled to check whether marks are too dense for the mark pass.
const SAMPLED_HAYSTACK_LEN: usize = 16 * 1024;

/// Splits a haystack's start positions into consecutive regions of `region_len`, the last
/// one possibly shorter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegionLayout {
    haystack_len: usize,
    region_len: usize,
}

impl RegionLayout {
    fn count(self) -> usize {
        self.haystack_len.div_ceil(self.region_len)
    }

    fn starts(self, region_index: usize) -> Range<usize> {
        let first_start = region_index * self.region_len;
        first_start..(first_start + self.region_len).min(self.haystack_len)
    }
}

impl ApproximateEngine {
    /// Builds an engine from raw patterns; `config` must set `max_edit_distance`.
    ///
    /// # Errors
    ///
    /// Returns `SearchError::EditDistanceTooLarge` if the distance exceeds `u32::MAX`.
    pub(crate) fn new(patterns: &[Vec<u8>], config: SearchConfig) -> Result<Self, SearchError> {
        let distance = config
            .max_edit_distance()
            .expect("approximate engine requires a configured edit distance");
        let max_edit_distance =
            u32::try_from(distance).map_err(|_| SearchError::EditDistanceTooLarge { distance })?;
        let patterns: Vec<ApproximatePattern> = patterns
            .iter()
            .map(|pattern| {
                let normalized = normalize_pattern(pattern.as_slice(), config.case_mode());
                ApproximatePattern::new(normalized, max_edit_distance)
            })
            .collect();

        Ok(Self {
            bitap: BitapPrefilter::build(&patterns, ignores_case(config)),
            marks: MarkPlan::new(&patterns, None),
            mark_choices: MarkPlan::choices(&patterns),
            patterns,
            max_edit_distance,
        })
    }

    /// Returns up to `limit` approximate matches in `haystack`, sorted by position.
    pub(crate) fn search(&self, haystack: &[u8], config: SearchConfig, limit: usize) -> Vec<Match> {
        self.search_scoped(haystack, config, limit, false)
    }

    /// Returns whether [`Self::search_line_local`] is available for every pattern.
    pub(crate) fn supports_line_local_search(&self) -> bool {
        self.patterns.iter().all(|pattern| pattern.halves.is_some())
    }

    /// Searches the whole haystack, admitting only windows within a single line. The result
    /// equals concatenating unlimited per-line searches, since windows never span lines.
    ///
    /// Callers must check [`Self::supports_line_local_search`].
    pub(crate) fn search_line_local(&self, haystack: &[u8], config: SearchConfig) -> Vec<Match> {
        self.search_scoped(haystack, config, usize::MAX, true)
    }

    fn search_scoped(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        limit: usize,
        line_local: bool,
    ) -> Vec<Match> {
        if limit == 0 || haystack.is_empty() {
            return Vec::new();
        }

        let ctx = PrefilterContext::new(haystack, config, line_local);
        let sampled = haystack.len() >= SAMPLED_HAYSTACK_LEN && !self.marks.groups().is_empty();
        let sample = sampled
            .then(|| PairSample::new(haystack, ctx.folds))
            .flatten();
        // Choosing marks costs a sample scan per candidate pair, which short haystacks don't
        // repay.
        let marks = match &sample {
            Some(sample) if haystack.len() >= self.mark_choices * sample.counted_positions() => {
                Cow::Owned(MarkPlan::new(&self.patterns, Some(sample)))
            }
            Some(_) | None => Cow::Borrowed(&self.marks),
        };
        let marks = marks.as_ref();
        if !self.supports_line_local_search() {
            let candidates = self.search_with_fallback(&ctx, marks);
            return select_approximate_matches(candidates, config, limit);
        }

        // The bit-parallel pass has fixed cost, while the mark pass costs per mark hit, so
        // text dense in marks goes to the bit-parallel pass.
        let bitap = self.bitap.as_deref().filter(|bitap| {
            sample
                .as_ref()
                .is_some_and(|sample| marks.stops_within(bitap.stop_spacing(), sample))
        });
        let regions = self.region_layout(haystack.len(), bitap.is_some());
        let search_region = |region_index: usize| {
            self.search_region(&ctx, bitap, marks, regions.starts(region_index))
        };
        let region_count = regions.count();
        if region_count == 1 {
            return select_approximate_matches(search_region(0), config, limit);
        }
        if !profiles::has_parallel_workers() {
            // Lazy iteration lets a limited search stop early.
            return select_approximate_matches(
                (0..region_count).flat_map(search_region),
                config,
                limit,
            );
        }
        if limit != usize::MAX {
            let runs = map_chunks(region_count, search_region);
            return select_approximate_matches(runs.into_iter().flatten(), config, limit);
        }
        if config.overlapping() {
            // Overlapping search selects every candidate, so regions have nothing to stitch.
            return concat_chunks(region_count, None, |region_index| {
                search_region(region_index)
                    .into_iter()
                    .map(|candidate| candidate.matched)
                    .collect()
            });
        }
        let mut matches = Vec::new();
        // Each region selects matches speculatively, as if nothing overlapped from the left.
        // Stitching replays only the picks affected by the previous region's last match.
        let mut last_end = 0;
        merge_chunks(
            region_count,
            |region_index| {
                let candidates = search_region(region_index);
                let speculative =
                    select_approximate_matches(candidates.iter().copied(), config, usize::MAX);
                (candidates, speculative)
            },
            |region_index, (candidates, speculative)| {
                if region_index == 0 {
                    // Only a hint: a replay may pick a few more than the speculative selection.
                    let every_region = speculative.len().saturating_mul(region_count);
                    reserve_for_other_chunks(&mut matches, every_region, None);
                }
                stitch_region_selection(&mut matches, &mut last_end, &candidates, &speculative);
            },
        );
        release_unused_capacity(&mut matches);
        matches
    }

    /// Plans counting matching lines with one mark pass and no full search, using `sample`
    /// (the input or its beginning) to choose marks.
    ///
    /// Returns `None` if lines must be counted by [`Self::count_searched_lines`]: in
    /// whole-word mode, for patterns without halves, or when one pass can't cover the halves
    /// (ambiguous, too many, or marks too dense).
    pub(crate) fn line_count_plan(
        &self,
        sample: &[u8],
        config: SearchConfig,
    ) -> Option<ApproximateLinePlan> {
        if config.whole_word() || !self.supports_line_local_search() {
            return None;
        }
        let insensitive = ignores_case(config);
        let sample = PairSample::new(sample, insensitive);
        let marks = MarkPlan::new(&self.patterns, sample.as_ref());
        // Dense marks are left to the search only if it has the bit-parallel pass.
        let dense = self
            .bitap
            .as_ref()
            .zip(sample)
            .is_some_and(|(bitap, sample)| marks.stops_within(bitap.stop_spacing(), &sample));
        let halves = marks.into_single_group()?;
        (!dense).then_some(ApproximateLinePlan { halves })
    }

    /// Counts the lines with a match of [`Self::search_line_local`] using `plan`, which must
    /// have been made with the same `config` for this input (or one it is whole lines of).
    pub(crate) fn count_planned_lines(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        plan: &ApproximateLinePlan,
    ) -> usize {
        if haystack.is_empty() {
            return 0;
        }
        let folds = ignores_case(config);
        // Cheap per byte, so this uses the mark pass's region sizes.
        let regions = self.region_layout(haystack.len(), false);
        let mut counted = CountedLines::default();
        for region in map_chunks(regions.count(), |region_index| {
            let owned = regions.starts(region_index);
            if folds {
                let text = SearchedText::<true> { bytes: haystack };
                count_marked_lines(text, owned, &self.patterns, &plan.halves)
            } else {
                let text = SearchedText::<false> { bytes: haystack };
                count_marked_lines(text, owned, &self.patterns, &plan.halves)
            }
        }) {
            counted.append(region, haystack);
        }
        counted.count()
    }

    /// Counts the lines with a match of [`Self::search_line_local`] by searching.
    pub(crate) fn count_searched_lines(&self, haystack: &[u8], config: SearchConfig) -> usize {
        count_matching_lines_from_global_matches(haystack, self.search_line_local(haystack, config))
    }

    /// Splits the starts of a haystack into regions of [`Self::region_len`].
    fn region_layout(&self, haystack_len: usize, bit_parallel: bool) -> RegionLayout {
        RegionLayout {
            haystack_len,
            region_len: self.region_len(haystack_len, bit_parallel),
        }
    }

    /// Returns the region length: the whole haystack if it's too small to repay waking the
    /// pool, `SEQUENTIAL_REGION_LEN` for a single-thread pool, or else a multiple of 64 that
    /// gives each thread several regions.
    fn region_len(&self, haystack_len: usize, bit_parallel: bool) -> usize {
        let parallel_len = if bit_parallel || self.patterns.len() > 1 {
            DENSE_PARALLEL_LEN
        } else {
            profiles::PARALLEL_APPROX_THRESHOLD
        };
        if haystack_len < parallel_len {
            return haystack_len;
        }
        if !profiles::has_parallel_workers() {
            return haystack_len.min(SEQUENTIAL_REGION_LEN);
        }
        let min_region_len = if bit_parallel {
            MIN_DENSE_REGION_LEN
        } else {
            MIN_SPARSE_REGION_LEN
        };
        let region_count = (haystack_len / min_region_len)
            .clamp(1, REGIONS_PER_THREAD * rayon::current_num_threads());
        haystack_len.div_ceil(region_count).next_multiple_of(64)
    }

    /// Returns the candidates starting in `owned`, sorted by `approximate_candidate_cmp`.
    /// With several patterns, only each start's best candidate is kept.
    fn search_region(
        &self,
        ctx: &PrefilterContext<'_>,
        bitap: Option<&BitapPrefilter>,
        marks: &MarkPlan,
        owned: Range<usize>,
    ) -> Vec<ApproximateCandidate> {
        if let Some(bitap) = bitap {
            let mut candidates = bitap.region_windows(ctx, owned);
            if ctx.config.whole_word() {
                candidates
                    .retain(|c| is_word_boundary(ctx.haystack, c.matched.start, c.matched.end));
            }
            sort_nearly_sorted(&mut candidates);
            if self.patterns.len() > 1 {
                candidates.dedup_by_key(|candidate| candidate.matched.start);
            }
            return candidates;
        }
        let region = StartRegion::new(owned, ctx.haystack.len());
        let mut per_pattern = marked_candidates(ctx, &region, &self.patterns, marks);
        if let [candidates] = per_pattern.as_mut_slice() {
            return std::mem::take(candidates);
        }
        let per_pattern = per_pattern
            .into_iter()
            .map(ApproximateCandidate::dedup_starts)
            .collect::<SmallVec<[_; 8]>>();
        merge_sorted_runs(per_pattern.iter().map(Vec::as_slice).collect())
    }

    /// Searches patterns without halves with SIMD Levenshtein (one task each) and the rest
    /// with the mark pass (one task per region).
    fn search_with_fallback(
        &self,
        ctx: &PrefilterContext<'_>,
        marks: &MarkPlan,
    ) -> Vec<ApproximateCandidate> {
        debug_assert!(
            !ctx.line_local,
            "line-local search requires patterns with halves"
        );
        let unhalved = self
            .patterns
            .iter()
            .enumerate()
            .filter(|(_, pattern)| pattern.halves.is_none())
            .map(|(pattern_index, _)| pattern_index)
            .collect::<SmallVec<[_; 8]>>();
        let regions = self.region_layout(ctx.haystack.len(), false);
        let region_count = if marks.groups().is_empty() {
            0
        } else {
            regions.count()
        };
        // The SIMD search compares bytes as they are, so it reads a lowered copy.
        let normalized = normalize_haystack(ctx.haystack, ctx.config.case_mode());
        let search = |task: usize| {
            if task < region_count {
                return self.search_region(ctx, None, marks, regions.starts(task));
            }
            let pattern_index = unhalved[task - region_count];
            let candidates = self.search_pattern_fallback(ctx, &normalized, pattern_index);
            ApproximateCandidate::dedup_starts(candidates)
        };
        let task_count = region_count + unhalved.len();
        let runs: Vec<_> = if profiles::approx_use_parallel_tasks(ctx.haystack.len(), task_count) {
            map_chunks(task_count, search)
        } else {
            (0..task_count).map(search).collect()
        };
        // Regions are consecutive, so their concatenation is already sorted.
        let (by_region, by_pattern) = runs.split_at(region_count);
        let by_marks = by_region.concat();
        let runs = std::iter::once(by_marks.as_slice()).chain(by_pattern.iter().map(Vec::as_slice));
        merge_sorted_runs(runs.collect())
    }

    /// Returns the first match in `haystack`, if any.
    pub(crate) fn find_first(&self, haystack: &[u8], config: SearchConfig) -> Option<Match> {
        self.search(haystack, config, 1).into_iter().next()
    }

    /// Searches the pattern at `pattern_index`, one without halves, using triple_accel SIMD
    /// Levenshtein on `normalized`, the context's haystack as the pattern compares with it.
    fn search_pattern_fallback(
        &self,
        ctx: &PrefilterContext<'_>,
        normalized: &[u8],
        pattern_index: usize,
    ) -> Vec<ApproximateCandidate> {
        let haystack = ctx.haystack;
        let mut candidates: Vec<ApproximateCandidate> = levenshtein_search_simd_with_opts(
            &self.patterns[pattern_index].bytes,
            normalized,
            self.max_edit_distance,
            SearchType::All,
            LEVENSHTEIN_COSTS,
            false,
        )
        .map(|matched| {
            ApproximateCandidate::new(matched.start, matched.end, pattern_index, matched.k)
        })
        .filter(|candidate| {
            candidate.matched.end <= haystack.len()
                && (!ctx.config.whole_word()
                    || is_word_boundary(haystack, candidate.matched.start, candidate.matched.end))
        })
        .collect();

        // triple_accel yields matches in end order; callers dedup starts after this sort.
        candidates.sort_unstable_by(approximate_candidate_cmp);
        candidates
    }
}

/// Counts distinct lines wholly containing at least one of `global_matches` (sorted by start).
pub(crate) fn count_matching_lines_from_global_matches(
    haystack: &[u8],
    global_matches: Vec<Match>,
) -> usize {
    if global_matches.is_empty() {
        return 0;
    }

    let mut count = 0_usize;
    let mut newline_iter = memchr_iter(b'\n', haystack);
    let mut line_end = newline_iter.next().unwrap_or(haystack.len());
    let mut counted_current_line = false;

    for candidate in global_matches {
        while candidate.start >= line_end {
            if line_end == haystack.len() {
                return count;
            }

            line_end = newline_iter.next().unwrap_or(haystack.len());
            counted_current_line = false;
        }

        if candidate.end > line_end || counted_current_line {
            continue;
        }

        count += 1;
        counted_current_line = true;
    }

    count
}

mod candidates;
mod prefilter;
#[cfg(test)]
mod tests;

use candidates::{
    CountedLines, MarkPlan, MarkedHalf, PackedPattern, SearchedText, StartRegion,
    approximate_candidate_cmp, count_marked_lines, marked_candidates, merge_sorted_runs,
    pattern_halves, select_approximate_matches, sort_nearly_sorted, stitch_region_selection,
};
use prefilter::BitapPrefilter;
