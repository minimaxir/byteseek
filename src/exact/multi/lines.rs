use aho_corasick::{Match as AhoMatch, packed};
use grep_matcher::Matcher;

use super::anchors::{ANCHOR_MIN_HAYSTACK_LEN, ANCHOR_SAMPLE_LEN, AnchoredSearcher};
use super::{MultiPatternEngine, next_line_start};
use crate::matcher::visit_candidate_lines;
use crate::{LineCounter, SearchConfig, is_word_boundary};

/// How [`MultiPatternEngine::visit_matching_lines_planned`] finds matching lines: with an
/// anchored searcher, or else the engine's own search.
pub(crate) struct LineCountPlan<'a>(pub(super) Option<AnchoredSearcher<'a>>);

/// Counts lines whose packed hits pass `verify(hit, origin)`, where `hit` is relative to
/// `origin`, passing `on_line` the offset of the first such hit in each. A rejected hit
/// resumes one byte later. An always-true `verify` inlines away.
#[inline(always)]
pub(super) fn count_packed_lines(
    searcher: &packed::Searcher,
    haystack: &[u8],
    mut verify: impl FnMut(AhoMatch, usize) -> bool,
    mut on_line: impl FnMut(usize),
) -> usize {
    let mut count = 0;
    let mut position = 0;
    while let Some(hit) = searcher.find(&haystack[position..]) {
        position = if verify(hit, position) {
            count += 1;
            on_line(position + hit.start());
            next_line_start(haystack, position + hit.end())
        } else {
            position + hit.start() + 1
        };
        if position == haystack.len() {
            break;
        }
    }
    count
}

impl MultiPatternEngine {
    /// Prepares counting matching lines of `haystack` or of its pieces: on a large haystack
    /// whose sample shows common pattern prefixes, packed search runs over rare anchors.
    pub(crate) fn line_count_plan(
        &self,
        haystack: &[u8],
        config: SearchConfig,
    ) -> LineCountPlan<'_> {
        let anchored = (!config.whole_word()
            && self.packed_leftmost_longest.is_some()
            && haystack.len() >= ANCHOR_MIN_HAYSTACK_LEN)
            .then(|| {
                AnchoredSearcher::new(
                    &self.packed_patterns,
                    &haystack[..ANCHOR_SAMPLE_LEN],
                    self.fold_haystack,
                )
            })
            .flatten();
        LineCountPlan(anchored)
    }

    #[cfg(test)]
    pub(crate) fn count_matching_lines(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        pattern_lengths: &[usize],
    ) -> usize {
        self.visit_matching_lines(haystack, config, pattern_lengths, |_| {})
    }

    /// Counts matching lines like [`Self::count_matching_lines`], passing `on_line` the offset
    /// of a match in each, in order.
    pub(crate) fn visit_matching_lines(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        pattern_lengths: &[usize],
        on_line: impl FnMut(usize),
    ) -> usize {
        let plan = self.line_count_plan(haystack, config);
        self.visit_matching_lines_planned(&plan, haystack, (config, pattern_lengths), on_line)
    }

    /// Counts matching lines of `haystack`, a piece of the haystack `plan` was made for,
    /// passing `on_line` the offset of a match in each, in order.
    pub(crate) fn visit_matching_lines_planned(
        &self,
        plan: &LineCountPlan<'_>,
        haystack: &[u8],
        (config, pattern_lengths): (SearchConfig, &[usize]),
        mut on_line: impl FnMut(usize),
    ) -> usize {
        if haystack.is_empty() {
            return 0;
        }

        if let Some(searcher) = &self.packed_leftmost_longest {
            let whole_word = config.whole_word();
            let mut count = 0_usize;
            self.for_each_packed_chunk(haystack, |offset, chunk| {
                let on_line = |at| on_line(offset + at);
                count += match &plan.0 {
                    // A line matches if any pattern occurs in it as a word: an occurrence that
                    // whole-word search passes over overlaps a match on the same line.
                    _ if whole_word => count_packed_lines(
                        searcher,
                        chunk,
                        |hit, origin| self.occurs_as_word(chunk, origin + hit.start()),
                        on_line,
                    ),
                    Some(anchored) => anchored.visit_lines(chunk, on_line),
                    None => count_packed_lines(searcher, chunk, |_, _| true, on_line),
                };
                true
            });
            return count;
        }
        if !config.whole_word() {
            return self.visit_matching_lines_line_matcher(haystack, on_line);
        }

        let mut counter = LineCounter::new(haystack);
        for matched in self.search(haystack, config, pattern_lengths, usize::MAX) {
            if counter.record(matched.start) {
                on_line(matched.start);
            }
        }
        counter.count()
    }

    /// Whether some pattern, as packed search sees it, occurs at `start` of `haystack`, as
    /// packed search sees it, between non-word bytes.
    fn occurs_as_word(&self, haystack: &[u8], start: usize) -> bool {
        self.packed_patterns.iter().any(|pattern| {
            let end = start + pattern.len();
            haystack.get(start..end) == Some(pattern.as_slice())
                && is_word_boundary(haystack, start, end)
        })
    }

    fn visit_matching_lines_line_matcher(
        &self,
        haystack: &[u8],
        mut on_line: impl FnMut(usize),
    ) -> usize {
        let line_matcher = self
            .line_matcher
            .as_ref()
            .expect("engines without a packed searcher build a line matcher");
        let mut count = 0;
        visit_candidate_lines(line_matcher, haystack, |kind, start, end| {
            let matched = match kind {
                grep_matcher::LineMatchKind::Confirmed(_) => true,
                grep_matcher::LineMatchKind::Candidate(_) => line_matcher
                    .is_match(&haystack[start..end])
                    .expect("literal line matcher should not fail"),
            };
            if matched {
                on_line(start);
            }
            count += usize::from(matched);
            true
        });
        count
    }
}
