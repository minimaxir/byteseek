use super::BooleanQueryEngine;
use crate::matcher::{line_content_end, visit_candidate_lines};
use crate::{Match, SearchConfig, line_end_from};

impl BooleanQueryEngine {
    /// Every term match in `haystack`, in start order, with the term's index as the pattern.
    pub(super) fn term_matches(&self, haystack: &[u8], config: SearchConfig) -> Vec<Match> {
        match &self.term_engine {
            Some(engine) => {
                engine.search(haystack, self.term_config, &self.term_lengths, usize::MAX)
            }
            None => self.term_checkers[0].search(haystack, config, usize::MAX),
        }
    }

    /// Visits every line with the terms present on it and their matches, for queries that can
    /// hold on lines without terms, and for line counts no clause plan covers.
    #[inline]
    pub(super) fn visit_term_lines(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        mut visit: impl FnMut(usize, usize, &[bool], &[Match]) -> bool,
    ) {
        let term_matches = self.term_matches(haystack, config);
        let mut present_terms = vec![false; self.term_lengths.len()];
        let mut touched_terms = Vec::with_capacity(self.term_lengths.len().min(16));
        let mut term_match_index = 0;
        let mut line_start = 0;
        while line_start < haystack.len() {
            let line_end = line_end_from(haystack, line_start);
            for term_index in touched_terms.drain(..) {
                present_terms[term_index] = false;
            }
            let line_terms_start = term_match_index;
            while term_match_index < term_matches.len()
                && term_matches[term_match_index].start < line_end
            {
                let term_index = term_matches[term_match_index].pattern_index;
                if !present_terms[term_index] {
                    present_terms[term_index] = true;
                    touched_terms.push(term_index);
                }
                term_match_index += 1;
            }
            if !visit(
                line_start,
                line_content_end(haystack, line_start, line_end),
                &present_terms,
                &term_matches[line_terms_start..term_match_index],
            ) || line_end == haystack.len()
            {
                break;
            }
            line_start = line_end + 1;
        }
    }

    #[inline]
    pub(super) fn count_lines_from_term_matches(
        &self,
        haystack: &[u8],
        config: SearchConfig,
    ) -> usize {
        let mut count = 0;
        self.visit_term_lines(haystack, config, |_, _, present_terms, _| {
            count += usize::from(
                self.patterns
                    .iter()
                    .any(|pattern| pattern.expression.evaluate(present_terms)),
            );
            true
        });
        count
    }

    /// Resolves each term at most once per candidate line, retaining expression short-circuiting.
    #[inline]
    pub(super) fn visit_candidate_matches(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        mut visit: impl FnMut(Match) -> bool,
    ) {
        let mut resolved_terms = vec![None; self.term_checkers.len()];
        let mut touched_terms = Vec::with_capacity(self.term_checkers.len().min(16));
        visit_candidate_lines(&self.line_matcher, haystack, |_, start, end| {
            for term_index in touched_terms.drain(..) {
                resolved_terms[term_index] = None;
            }
            let mut resolve = |term_index: usize| {
                if let Some(value) = resolved_terms[term_index] {
                    return value;
                }
                let value = self.term_checkers[term_index]
                    .contains_line_term(&haystack[start..end], config);
                resolved_terms[term_index] = Some(value);
                touched_terms.push(term_index);
                value
            };
            self.patterns
                .iter()
                .find_map(|pattern| {
                    pattern
                        .expression
                        .evaluate_with(&mut resolve)
                        .then_some(pattern.pattern_index)
                })
                .is_none_or(|pattern_index| {
                    visit(Match {
                        start,
                        end,
                        pattern_index,
                    })
                })
        });
    }
}
