use memchr::memmem;
use memchr::{memchr_iter, memchr2_iter};

use super::{BlockPlan, LiteralScanner, STOP};
use crate::profiles;
use crate::{
    CaseMode, LineCounter, Match, SearchConfig, fold_ascii, is_word_boundary, select_anchor,
    unfold_ascii,
};

mod scan;

/// Counts up to `limit` non-overlapping occurrences of `finder`'s needle in `haystack`. Kept
/// out of line so the per-match loop does not share registers with a large caller.
#[inline(never)]
fn count_occurrences(finder: &memmem::Finder<'_>, haystack: &[u8], limit: usize) -> usize {
    finder.find_iter(haystack).take(limit).count()
}

fn single_match(start: usize, end: usize) -> Match {
    Match {
        start,
        end,
        pattern_index: 0,
    }
}

#[inline(always)]
fn visit_accepted_single_byte_positions(
    haystack: &[u8],
    whole_word: bool,
    positions: impl Iterator<Item = usize>,
    mut visit: impl FnMut(usize) -> bool,
) {
    for start in positions {
        if whole_word && !is_word_boundary(haystack, start, start + 1) {
            continue;
        }
        if !visit(start) {
            break;
        }
    }
}

#[inline(always)]
fn visit_single_byte_candidates(
    haystack: &[u8],
    target: u8,
    config: SearchConfig,
    visit: impl FnMut(usize) -> bool,
) {
    let (lower, upper) = match config.case_mode() {
        CaseMode::Sensitive => (target, target),
        CaseMode::AsciiInsensitive => {
            let lower = fold_ascii(target);
            (lower, unfold_ascii(lower))
        }
    };
    if lower == upper {
        visit_accepted_single_byte_positions(
            haystack,
            config.whole_word(),
            memchr_iter(lower, haystack),
            visit,
        );
    } else {
        visit_accepted_single_byte_positions(
            haystack,
            config.whole_word(),
            memchr2_iter(lower, upper, haystack),
            visit,
        );
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SinglePatternEngine {
    /// Block scanners for long haystacks: case-sensitive and ASCII-insensitive.
    scanners: Option<[LiteralScanner; 2]>,
    pattern: Vec<u8>,
    finder: memmem::Finder<'static>,
    anchor: (usize, u8),
    shift: [usize; 256],
}

impl SinglePatternEngine {
    pub(crate) fn new(pattern: Vec<u8>) -> Self {
        let mut shift = [pattern.len(); 256];
        if pattern.len() > 1 {
            let last = pattern.len() - 1;
            for (index, byte) in pattern.iter().take(last).copied().enumerate() {
                shift[usize::from(byte)] = last - index;
            }
        }
        let finder = memmem::Finder::new(pattern.as_slice()).into_owned();
        let anchor = select_anchor(pattern.as_slice());
        // Single bytes use byte search.
        let scanner = |case_mode| LiteralScanner::new(std::slice::from_ref(&pattern), case_mode);
        let scanners = (pattern.len() > 1)
            .then(|| scanner(CaseMode::Sensitive).zip(scanner(CaseMode::AsciiInsensitive)))
            .flatten()
            .map(<[LiteralScanner; 2]>::from);
        Self {
            scanners,
            pattern,
            finder,
            anchor,
            shift,
        }
    }

    /// The pattern byte anchored searches look for: its first least frequent byte.
    pub(crate) fn anchor_byte(&self) -> u8 {
        self.anchor.1
    }

    #[inline]
    pub(crate) fn search(&self, haystack: &[u8], config: SearchConfig, limit: usize) -> Vec<Match> {
        self.search_planned(haystack, config, limit, self.block_plan(haystack, config))
    }

    /// Like [`Self::search`], but with a block plan shared by all pieces of a split input.
    #[inline]
    pub(crate) fn search_planned(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        limit: usize,
        planned: BlockPlan<'_>,
    ) -> Vec<Match> {
        if limit == usize::MAX {
            self.collect_matches::<false>(haystack, config, limit, planned)
        } else {
            self.collect_matches::<true>(haystack, config, limit, planned)
        }
    }

    #[inline]
    fn collect_matches<const LIMITED: bool>(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        limit: usize,
        planned: BlockPlan<'_>,
    ) -> Vec<Match> {
        let pattern_len = self.pattern.len();
        if limit == 0 || haystack.len() < pattern_len {
            return Vec::new();
        }

        let profile = profiles::single_profile(config, haystack.len(), pattern_len, limit);
        let capacity = if let Some((_, plan)) = &planned {
            // The sampled density, floored at the length-based estimate and capped at what
            // fits.
            let most = if config.overlapping() {
                profiles::OVERLAPPING_CAPACITY_CEILING
            } else {
                haystack.len() / pattern_len
            };
            plan.expected_occurrences(haystack.len())
                .min(most)
                .max(profile.initial_capacity)
                .min(limit)
        } else if !LIMITED && config.overlapping() {
            self.sampled_overlapping_capacity(haystack, config, profile.initial_capacity)
        } else {
            profile.initial_capacity
        };
        let mut matches = Vec::with_capacity(capacity);
        // Preserve the unlimited iterator path without a per-match limit branch.
        if planned.is_none()
            && !LIMITED
            && pattern_len > 1
            && profile.strategy == profiles::SingleStrategy::SensitiveMemmem
            && !config.whole_word()
        {
            matches.extend(
                self.finder
                    .find_iter(haystack)
                    .map(|start| single_match(start, start + pattern_len)),
            );
            return matches;
        }
        let collect = |matched| {
            matches.push(matched);
            !LIMITED || matches.len() != limit
        };
        self.visit_blocks(haystack, config, (profile.strategy, planned), collect);
        matches
    }

    /// The block scanner for `config` with its plan for `haystack`, if usable: patterns of
    /// two or more bytes, long haystacks, and no word boundaries.
    #[inline]
    pub(crate) fn block_plan(&self, haystack: &[u8], config: SearchConfig) -> BlockPlan<'_> {
        if config.whole_word() {
            return None;
        }
        let [sensitive, insensitive] = self.scanners.as_ref()?;
        let scanner = match config.case_mode() {
            CaseMode::Sensitive => sensitive,
            CaseMode::AsciiInsensitive => insensitive,
        };
        Some((scanner, scanner.plan(haystack)?))
    }

    /// Like [`Self::visit_matches`], but block-scans with the plan as far as it goes and
    /// uses `strategy` for the rest. Returning `false` from `visit` stops the search.
    #[inline(always)]
    fn visit_blocks(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        (strategy, planned): (profiles::SingleStrategy, BlockPlan<'_>),
        mut visit: impl FnMut(Match) -> bool,
    ) {
        let Some((scanner, plan)) = planned else {
            self.visit_matches::<true>(haystack, config, strategy, visit);
            return;
        };
        let overlapping = config.overlapping();
        let gave_up = scanner.scan(&plan, haystack, |matched| {
            if !visit(matched) {
                STOP
            } else if overlapping {
                matched.start + 1
            } else {
                matched.end
            }
        });
        let Some(resume) = gave_up else {
            return;
        };
        self.visit_matches::<true>(&haystack[resume..], config, strategy, |matched| {
            visit(single_match(matched.start + resume, matched.end + resume))
        });
    }

    /// Capacity for an unlimited overlapping search: `estimate`, raised to the density of a
    /// sampled prefix of a long `haystack`, since repeated regrowth costs more than sampling.
    fn sampled_overlapping_capacity(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        estimate: usize,
    ) -> usize {
        const SAMPLE_LEN: usize = 1024;
        const MIN_HAYSTACK_LEN: usize = 16 * SAMPLE_LEN;

        if haystack.len() < MIN_HAYSTACK_LEN {
            return estimate;
        }
        let sampled = self.count(&haystack[..SAMPLE_LEN], config, usize::MAX);
        // One sixteenth of slack absorbs text slightly denser than its sample.
        let expected = sampled * (haystack.len() / SAMPLE_LEN);
        (expected + expected / 16)
            .min(profiles::OVERLAPPING_CAPACITY_CEILING)
            .max(estimate)
    }

    /// Visits every match in order, as an unlimited [`Self::search`] would collect them,
    /// using `planned` from [`Self::block_plan`]. Returning `false` from `visit` stops.
    pub(crate) fn visit_planned(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        planned: BlockPlan<'_>,
        visit: impl FnMut(Match) -> bool,
    ) {
        let strategy =
            profiles::single_profile(config, haystack.len(), self.pattern.len(), usize::MAX)
                .strategy;
        self.visit_blocks(haystack, config, (strategy, planned), visit);
    }

    pub(crate) fn find_first(&self, haystack: &[u8], config: SearchConfig) -> Option<Match> {
        let strategy =
            profiles::single_profile(config, haystack.len(), self.pattern.len(), 1).strategy;
        let mut first = None;
        self.visit_matches::<false>(haystack, config, strategy, |matched| {
            first = Some(matched);
            false
        });
        first
    }

    pub(crate) fn contains(&self, haystack: &[u8], config: SearchConfig) -> bool {
        self.find_first(haystack, config).is_some()
    }

    /// Counts up to `limit` matches without materializing them, agreeing with
    /// [`Self::search`].
    pub(crate) fn count(&self, haystack: &[u8], config: SearchConfig, limit: usize) -> usize {
        self.count_planned(haystack, config, limit, self.block_plan(haystack, config))
    }

    /// Like [`Self::count`], with a block plan shared by all pieces of a split input.
    pub(crate) fn count_planned(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        limit: usize,
        planned: BlockPlan<'_>,
    ) -> usize {
        if limit == 0 || haystack.len() < self.pattern.len() {
            return 0;
        }

        if planned.is_none()
            && self.pattern.len() > 1
            && config.case_mode() == CaseMode::Sensitive
            && !config.whole_word()
            && !config.overlapping()
        {
            return count_occurrences(&self.finder, haystack, limit);
        }

        let strategy =
            profiles::single_profile(config, haystack.len(), self.pattern.len(), usize::MAX)
                .strategy;
        let mut count = 0_usize;
        self.visit_blocks(haystack, config, (strategy, planned), |_| {
            count += 1;
            count != limit
        });
        count
    }

    pub(crate) fn contains_line_term(&self, haystack: &[u8], config: SearchConfig) -> bool {
        if config.case_mode() == CaseMode::Sensitive && !config.whole_word() {
            return self.finder.find(haystack).is_some();
        }

        self.contains(haystack, config)
    }

    /// Returns the raw (normalized) pattern bytes.
    pub(crate) fn pattern_bytes(&self) -> &[u8] {
        &self.pattern
    }

    pub(crate) fn count_matching_lines(&self, haystack: &[u8], config: SearchConfig) -> usize {
        self.visit_matching_lines(haystack, config, |_| {})
    }

    /// Counts the matching lines of `haystack` like [`Self::count_matching_lines`], passing
    /// `on_line` the offset of a match in each, in order.
    pub(crate) fn visit_matching_lines(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        mut on_line: impl FnMut(usize),
    ) -> usize {
        if self.pattern.len() > 1
            && config.case_mode() == CaseMode::Sensitive
            && !config.whole_word()
            && !config.overlapping()
        {
            let mut counter = LineCounter::new(haystack);
            for start in self.finder.find_iter(haystack) {
                if counter.record_valid_start(start) {
                    on_line(start);
                }
            }
            return counter.count();
        }
        self.visit_matching_lines_configured(haystack, config, on_line)
    }

    // Keep configuration-specialized scans out of the common native counting kernel.
    #[inline(never)]
    fn visit_matching_lines_configured(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        mut on_line: impl FnMut(usize),
    ) -> usize {
        let mut counter = LineCounter::new(haystack);
        // Line counting uses anchors even when result collection would fold the whole input.
        let record = |matched: Match| {
            if counter.record(matched.start) {
                on_line(matched.start);
            }
            true
        };
        let strategy = if config.case_mode() == CaseMode::AsciiInsensitive {
            profiles::SingleStrategy::AsciiAnchored
        } else {
            profiles::single_profile(config, haystack.len(), self.pattern.len(), 1).strategy
        };
        self.visit_matches::<false>(haystack, config, strategy, record);
        counter.count()
    }
}

#[cfg(test)]
mod tests;
