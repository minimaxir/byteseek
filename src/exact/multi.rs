use std::ops::Range;
use std::sync::OnceLock;

use aho_corasick::{AhoCorasick, Match as AhoMatch, packed};
use grep_regex::RegexMatcher;
use memchr::{memchr, memrchr};
use smallvec::SmallVec;

use super::{BlockPlan, LiteralScanner, STOP, collect_up_to, count_up_to};
use crate::profiles;
use crate::{
    CaseMode, Match, SearchConfig, SearchError, build_automaton, build_line_matcher,
    flush_multi_group, fold_ascii_bytes, is_word_boundary, line_end_from,
    select_best_pending_match,
};

/// Bytes folded per chunk for ASCII-insensitive packed search; chunks extend to a line end.
const FOLDED_CHUNK_LEN: usize = 32 * 1024;

/// Bytes per line-aligned block over which whole-word search chooses between searching only
/// the matching lines and searching the block whole.
const WORD_BLOCK_LEN: usize = 32 * 1024;

/// Compares per position at which the block scan costs about as much as packed search's
/// table lookups.
const PACKED_PARITY_COMPARES: usize = 8;
/// Largest average match spacing at which the block scan beats packed search regardless
/// of its compares.
const DENSE_MATCH_SPACING: usize = 32;

/// The content of the line around `at`, without its line feed.
#[inline]
fn line_around(haystack: &[u8], at: usize) -> Range<usize> {
    memrchr(b'\n', &haystack[..at]).map_or(0, |end| end + 1)..line_end_from(haystack, at)
}

/// Bytes of content on the lines of `haystack` that hold `matches`, which are in order and
/// span no line feed.
fn matching_line_bytes(haystack: &[u8], matches: &[Match]) -> usize {
    let mut line_end = 0;
    let mut bytes = 0;
    for found in matches {
        if found.start >= line_end {
            let line = line_around(haystack, found.start);
            line_end = line.end;
            bytes += line.len();
        }
    }
    bytes
}

#[inline]
fn next_line_start(haystack: &[u8], match_end: usize) -> usize {
    memchr(b'\n', &haystack[match_end..])
        .map_or(haystack.len(), |relative| match_end + relative + 1)
}

#[inline(always)]
fn candidate_match(candidate: AhoMatch) -> Match {
    candidate_match_at(candidate, 0)
}

#[inline(always)]
fn candidate_match_at(candidate: AhoMatch, offset: usize) -> Match {
    Match {
        start: offset + candidate.start(),
        end: offset + candidate.end(),
        pattern_index: candidate.pattern().as_usize(),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MultiPatternEngine {
    /// Standard-semantics automaton. With a packed searcher it serves only overlapping,
    /// whole-word, and parallel-boundary paths, so it is then built on first use.
    standard: OnceLock<AhoCorasick>,
    leftmost_longest: Option<AhoCorasick>,
    /// Leftmost-longest SIMD searcher. When `fold_haystack`, it holds folded patterns and
    /// searches folded line-aligned chunks.
    packed_leftmost_longest: Option<Box<packed::Searcher>>,
    /// The patterns `packed_leftmost_longest` searches for, folded like it when needed.
    packed_patterns: Vec<Vec<u8>>,
    fold_haystack: bool,
    /// Block scanner over `packed_patterns` for small sets; folds without copying.
    scanner: Option<LiteralScanner>,
    config: SearchConfig,
    /// Literal line matcher for line counting without a packed searcher.
    line_matcher: Option<RegexMatcher>,
}

impl MultiPatternEngine {
    pub(crate) fn new(patterns: &[Vec<u8>], config: SearchConfig) -> Result<Self, SearchError> {
        let packed_leftmost_longest = build_packed_searcher(patterns, config);
        let (standard, leftmost_longest, line_matcher) = if packed_leftmost_longest.is_some() {
            (OnceLock::new(), None, None)
        } else {
            // Eager construction surfaces automaton errors from `new`.
            (
                OnceLock::from(build_automaton(
                    patterns,
                    config,
                    aho_corasick::MatchKind::Standard,
                )?),
                build_automaton(patterns, config, aho_corasick::MatchKind::LeftmostLongest).ok(),
                Some(build_line_matcher(patterns, config)?),
            )
        };
        let packed_patterns = match (&packed_leftmost_longest, config.case_mode()) {
            (None, _) => Vec::new(),
            (Some(_), CaseMode::Sensitive) => patterns.to_vec(),
            (Some(_), CaseMode::AsciiInsensitive) => patterns
                .iter()
                .map(|pattern| fold_ascii_bytes(pattern))
                .collect(),
        };

        let scanner = packed_leftmost_longest
            .as_ref()
            .and_then(|_| LiteralScanner::new(&packed_patterns, config.case_mode()));

        Ok(Self {
            scanner,
            packed_patterns,
            standard,
            leftmost_longest,
            fold_haystack: packed_leftmost_longest.is_some()
                && config.case_mode() == CaseMode::AsciiInsensitive,
            packed_leftmost_longest,
            config,
            line_matcher,
        })
    }

    /// The block scanner and its plan for `haystack`, if it beats packed search. Only for
    /// small sets, long haystacks, and non-overlapping searches without word boundaries.
    #[inline]
    pub(crate) fn block_plan(&self, haystack: &[u8], config: SearchConfig) -> BlockPlan<'_> {
        if config.overlapping() || config.whole_word() {
            return None;
        }
        let scanner = self.scanner.as_ref()?;
        let plan = scanner.plan(haystack)?;
        // Packed search costs little per byte but restarts at every match; the block scan
        // never stops but pays per probe. So it wins with few probes or dense matches.
        (scanner.compares_per_position(&plan) <= PACKED_PARITY_COMPARES
            || plan.is_denser_than(DENSE_MATCH_SPACING))
        .then_some((scanner, plan))
    }

    #[inline]
    fn standard(&self) -> &AhoCorasick {
        // Packed-eligible pattern sets are a few short literals, far below automaton limits,
        // and case folding commutes with ASCII-insensitive automaton construction.
        self.standard.get_or_init(|| {
            build_automaton(
                &self.packed_patterns,
                self.config,
                aho_corasick::MatchKind::Standard,
            )
            .expect("packed-eligible patterns always fit a standard automaton")
        })
    }

    #[inline]
    fn leftmost_longest(&self) -> &AhoCorasick {
        self.leftmost_longest
            .as_ref()
            .unwrap_or_else(|| self.standard())
    }

    pub(crate) fn search(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        pattern_lengths: &[usize],
        limit: usize,
    ) -> Vec<Match> {
        if limit == 0 || haystack.is_empty() {
            return Vec::new();
        }

        if !config.overlapping() && !config.whole_word() {
            let planned = self.block_plan(haystack, config);
            return self.search_non_overlapping(haystack, pattern_lengths, limit, planned);
        }

        let min_pattern_len = pattern_lengths.iter().copied().min().unwrap_or(1);
        let mut matches = Vec::with_capacity(profiles::multi_initial_capacity(
            haystack.len(),
            min_pattern_len,
            limit,
        ));
        let mut last_end = 0_usize;
        // Offsets `pending` from a slice starting at `offset` into the haystack, then keeps
        // the group's choice.
        let mut flush = |pending: &mut SmallVec<[Match; 4]>, offset, matches: &mut Vec<Match>| {
            for found in pending.iter_mut() {
                found.start += offset;
                found.end += offset;
            }
            !flush_multi_group(
                pending,
                haystack,
                config,
                pattern_lengths,
                matches,
                &mut last_end,
                limit,
            )
        };
        let crosses_lines = self
            .packed_patterns
            .iter()
            .any(|pattern| pattern.contains(&b'\n'));
        if config.overlapping()
            || crosses_lines
            || self.packed_leftmost_longest.is_none()
            || limit != usize::MAX
        {
            self.visit_groups(haystack, |pending| flush(pending, 0, &mut matches));
            return matches;
        }

        // Whole-word matches lie on lines that hold some pattern as a word, and groups never
        // span lines, so a block is searched only on those lines while they are sparse.
        // Finding them costs a good part of a whole search, and each line adds a search's
        // setup, so once they fill a quarter of a block the next is searched whole.
        let mut sparse = true;
        let mut block_start = 0;
        while block_start < haystack.len() {
            let block_end =
                next_line_start(haystack, (block_start + WORD_BLOCK_LEN).min(haystack.len()));
            let block = &haystack[block_start..block_end];
            let first = matches.len();
            let line_bytes = if sparse {
                let mut line_bytes = 0;
                self.visit_matching_lines(block, config, pattern_lengths, |at| {
                    let line = line_around(block, at);
                    line_bytes += line.len();
                    self.visit_groups(&block[line.clone()], |pending| {
                        flush(pending, block_start + line.start, &mut matches)
                    });
                });
                line_bytes
            } else {
                self.visit_groups(block, |pending| flush(pending, block_start, &mut matches));
                matching_line_bytes(haystack, &matches[first..])
            };
            sparse = 4 * line_bytes < block.len();
            block_start = block_end;
        }
        matches
    }

    pub(crate) fn find_first(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        pattern_lengths: &[usize],
    ) -> Option<Match> {
        if haystack.is_empty() {
            return None;
        }

        if !config.whole_word() {
            if !config.overlapping()
                && let Some(searcher) = &self.packed_leftmost_longest
            {
                let mut first = None;
                self.for_each_packed_chunk(haystack, |offset, chunk| {
                    first = searcher
                        .find(chunk)
                        .map(|candidate| candidate_match_at(candidate, offset));
                    first.is_none()
                });
                return first;
            }

            let candidate = if config.overlapping() {
                self.standard().find(haystack)
            } else {
                self.leftmost_longest().find(haystack)
            };

            return candidate.map(candidate_match);
        }

        if config.overlapping() {
            return self
                .standard()
                .find_overlapping_iter(haystack)
                .find_map(|candidate| {
                    let matched = candidate_match(candidate);
                    is_word_boundary(haystack, matched.start, matched.end).then_some(matched)
                });
        }

        let mut first = None;
        self.visit_groups(haystack, |pending| {
            first = select_best_pending_match(pending, haystack, config, pattern_lengths);
            first.is_none()
        });
        first
    }

    pub(crate) fn contains(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        pattern_lengths: &[usize],
    ) -> bool {
        if config.whole_word() || self.packed_leftmost_longest.is_some() {
            self.find_first(haystack, config, pattern_lengths).is_some()
        } else {
            self.leftmost_longest().is_match(haystack)
        }
    }

    /// Counts up to `limit` leftmost-longest matches, block-scanning with `planned` (from
    /// [`Self::block_plan`]) as far as it goes. Returns `None` for overlapping or whole-word
    /// search.
    pub(crate) fn count_planned(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        limit: usize,
        planned: BlockPlan<'_>,
    ) -> Option<usize> {
        if limit == 0 || haystack.is_empty() {
            return Some(0);
        }
        if config.overlapping() || config.whole_word() {
            return None;
        }

        let mut count = 0_usize;
        let Some(resume) = scan_planned(haystack, planned, |matched| {
            count += 1;
            if count == limit { STOP } else { matched.end }
        }) else {
            return Some(count);
        };
        let rest = &haystack[resume..];
        let Some(searcher) = &self.packed_leftmost_longest else {
            let remaining = limit - count;
            return Some(count + count_up_to(self.leftmost_longest().find_iter(rest), remaining));
        };
        // The common sensitive case counts native hits without offsets or a Match visitor.
        if !self.fold_haystack {
            return Some(count + count_up_to(searcher.find_iter(&rest), limit - count));
        }
        self.for_each_packed_chunk(rest, |_, chunk| {
            count += count_up_to(searcher.find_iter(&chunk), limit - count);
            count < limit
        });
        Some(count)
    }

    /// Visits leftmost-longest matches in order until `visit` returns `false`, block-scanning
    /// with `planned` as far as it goes and searching the rest normally.
    #[inline]
    pub(crate) fn visit_planned(
        &self,
        haystack: &[u8],
        planned: BlockPlan<'_>,
        mut visit: impl FnMut(Match) -> bool,
    ) {
        let Some(resume) = scan_planned(haystack, planned, |matched| {
            if visit(matched) { matched.end } else { STOP }
        }) else {
            return;
        };
        let rest = &haystack[resume..];
        if let Some(searcher) = &self.packed_leftmost_longest {
            self.for_each_packed_chunk(rest, |offset, chunk| {
                searcher
                    .find_iter(&chunk)
                    .all(|candidate| visit(candidate_match_at(candidate, resume + offset)))
            });
            return;
        }

        for candidate in self.leftmost_longest().find_iter(rest) {
            if !visit(candidate_match_at(candidate, resume)) {
                break;
            }
        }
    }

    pub(crate) fn boundary_is_safe(
        &self,
        haystack: &[u8],
        boundary: usize,
        max_pattern_len: usize,
    ) -> bool {
        if boundary == 0 || boundary == haystack.len() {
            return true;
        }

        let overlap = max_pattern_len.saturating_sub(1);
        let window_start = boundary.saturating_sub(overlap);
        let window_end = boundary.saturating_add(overlap).min(haystack.len());
        self.standard()
            .find_overlapping_iter(&haystack[window_start..window_end])
            .all(|candidate| {
                let start = window_start + candidate.start();
                let end = window_start + candidate.end();
                start >= boundary || end <= boundary
            })
    }

    /// Collects up to `limit` leftmost-longest matches in order, block-scanning with
    /// `planned` as far as it goes. `pattern_lengths` sizes the initial reservation.
    pub(crate) fn search_non_overlapping(
        &self,
        haystack: &[u8],
        pattern_lengths: &[usize],
        limit: usize,
        planned: BlockPlan<'_>,
    ) -> Vec<Match> {
        if limit == 0 || haystack.is_empty() {
            return Vec::new();
        }
        let min_pattern_len = pattern_lengths.iter().copied().min().unwrap_or(1);
        let capacity = profiles::multi_initial_capacity(haystack.len(), min_pattern_len, limit);
        let mut matches = Vec::new();
        if let Some((_, plan)) = &planned {
            // The sampled density sizes the result better than lengths alone can.
            let expected = plan.expected_occurrences(haystack.len());
            let most = haystack.len() / min_pattern_len.max(1);
            matches.reserve(expected.min(most).max(capacity).min(limit));
        }
        let Some(resume) = scan_planned(haystack, planned, |matched| {
            matches.push(matched);
            if matches.len() == limit {
                STOP
            } else {
                matched.end
            }
        }) else {
            return matches;
        };
        let rest = &haystack[resume..];
        let Some(searcher) = &self.packed_leftmost_longest else {
            let candidates = self.leftmost_longest().find_iter(rest);
            if matches.capacity() == 0 {
                return collect_up_to(candidates.map(candidate_match), capacity, limit);
            }
            extend_up_to(&mut matches, candidates, resume, limit);
            return matches;
        };
        if !self.fold_haystack {
            if matches.capacity() == 0 {
                return collect_up_to(
                    searcher.find_iter(&rest).map(candidate_match),
                    capacity,
                    limit,
                );
            }
            extend_up_to(&mut matches, searcher.find_iter(&rest), resume, limit);
            return matches;
        }
        matches.reserve(capacity.saturating_sub(matches.len()).min(limit));
        self.for_each_packed_chunk(rest, |offset, chunk| {
            extend_up_to(
                &mut matches,
                searcher.find_iter(&chunk),
                resume + offset,
                limit,
            );
            matches.len() < limit
        });
        matches
    }
}

/// Block-scans `haystack` with `planned`, handing matches to `visit` (which returns the
/// next start or [`STOP`]). Returns where the other searchers must resume: zero without a
/// plan, the give-up offset, or `None` if nothing is left.
#[inline]
fn scan_planned(
    haystack: &[u8],
    planned: BlockPlan<'_>,
    visit: impl FnMut(Match) -> usize,
) -> Option<usize> {
    let Some((scanner, plan)) = planned else {
        return Some(0);
    };
    scanner.scan(&plan, haystack, visit)
}

mod anchors;
mod lines;
mod scan;
#[cfg(test)]
mod tests;

pub(crate) use lines::LineCountPlan;
use scan::{build_packed_searcher, extend_up_to};
