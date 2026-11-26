use memchr::{memchr_iter, memrchr};
use smallvec::SmallVec;

use super::{BooleanClause, BooleanQueryEngine};
use crate::matcher::line_content_end;
use crate::{SearchConfig, line_end_from};

/// Haystacks at least this large sample term selectivity before counting clause lines.
const PLANNER_MIN_HAYSTACK_LEN: usize = 64 * 1024;
const PLANNER_SAMPLE_WINDOWS: usize = 4;
const PLANNER_SAMPLE_WINDOW_LEN: usize = 2 * 1024;

/// Evaluation order for one conjunctive clause: lines holding the anchor term are checked
/// against the remaining terms.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClausePlan {
    anchor_term_index: usize,
    /// `(term_index, must_be_present)` pairs, most likely to reject a line first.
    checks: SmallVec<[(usize, bool); 8]>,
}

/// Clause plans sampled from a whole haystack and shared by its line-aligned chunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BooleanLinePlan {
    clauses: Vec<ClausePlan>,
}

impl BooleanQueryEngine {
    pub(super) fn count_clause_lines(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        clauses: &[BooleanClause],
    ) -> usize {
        match self.plan_clauses(haystack, config, clauses) {
            Some(plan) => self.count_planned_lines(haystack, config, &plan),
            None => self.count_lines_from_term_matches(haystack, config),
        }
    }

    /// Plans line counting over `haystack` for reuse on its chunks. Returns `None` when
    /// counting does not use clause plans.
    pub(crate) fn line_count_plan(
        &self,
        haystack: &[u8],
        config: SearchConfig,
    ) -> Option<BooleanLinePlan> {
        if self.can_match_without_terms {
            return None;
        }
        self.plan_clauses(haystack, config, self.line_count_clauses.as_deref()?)
    }

    /// Counts matching lines of `chunk`, a line-aligned part of the haystack `plan` is for.
    pub(crate) fn count_planned_lines(
        &self,
        chunk: &[u8],
        config: SearchConfig,
        plan: &BooleanLinePlan,
    ) -> usize {
        if let [clause] = plan.clauses.as_slice() {
            let mut count = 0_usize;
            self.visit_clause_lines(chunk, config, clause, |_| count += 1);
            return count;
        }

        let mut matched_line_starts = Vec::new();
        for clause in &plan.clauses {
            self.visit_clause_lines(chunk, config, clause, |line_start| {
                matched_line_starts.push(line_start);
            });
        }
        // Each clause adds sorted runs, which the stable sort merges cheaply.
        matched_line_starts.sort();
        matched_line_starts.dedup();
        matched_line_starts.len()
    }

    fn plan_clauses(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        clauses: &[BooleanClause],
    ) -> Option<BooleanLinePlan> {
        let clauses = clauses
            .iter()
            .map(|clause| self.plan_clause(haystack, config, clause))
            .collect::<Option<Vec<_>>>()?;
        Some(BooleanLinePlan { clauses })
    }

    /// Orders a clause's term checks by selectivity, sampling large haystacks to anchor on the
    /// rarest positive term; small ones anchor on the longest. Returns `None` for clauses
    /// without a positive term.
    fn plan_clause(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        clause: &BooleanClause,
    ) -> Option<ClausePlan> {
        let positive_terms = clause.positive_terms.as_slice();
        let negative_terms = clause.negative_terms.as_slice();
        let longest_positive = positive_terms
            .iter()
            .copied()
            .max_by_key(|&term_index| self.term_lengths[term_index])?;

        if haystack.len() < PLANNER_MIN_HAYSTACK_LEN {
            let checks = positive_terms
                .iter()
                .filter(|&&term_index| term_index != longest_positive)
                .map(|&term_index| (term_index, true))
                .chain(negative_terms.iter().map(|&term_index| (term_index, false)))
                .collect();
            return Some(ClausePlan {
                anchor_term_index: longest_positive,
                checks,
            });
        }

        let samples = sample_windows(haystack);
        let sampled_lines = samples
            .iter()
            .map(|window| memchr_iter(b'\n', window).count() + 1)
            .sum::<usize>();
        let line_hits = |term_index: usize| {
            samples
                .iter()
                .map(|window| self.term_checkers[term_index].count_matching_lines(window, config))
                .sum::<usize>()
        };
        // (term, must_be_present, sampled lines on which the check rejects)
        let mut weighted = positive_terms
            .iter()
            .map(|&term_index| {
                let hits = line_hits(term_index);
                (term_index, true, sampled_lines.saturating_sub(hits))
            })
            .chain(
                negative_terms
                    .iter()
                    .map(|&term_index| (term_index, false, line_hits(term_index))),
            )
            .collect::<SmallVec<[(usize, bool, usize); 8]>>();

        // The anchor rejects the most lines without being checked; ties prefer longer terms.
        let (anchor_position, &(anchor_term_index, _, _)) = weighted
            .iter()
            .enumerate()
            .filter(|(_, (_, must_be_present, _))| *must_be_present)
            .max_by_key(|(_, (term_index, _, rejected))| {
                (*rejected, self.term_lengths[*term_index])
            })?;
        weighted.remove(anchor_position);
        weighted.sort_by_key(|&(_, _, rejected)| std::cmp::Reverse(rejected));

        Some(ClausePlan {
            anchor_term_index,
            checks: weighted
                .into_iter()
                .map(|(term_index, must_be_present, _)| (term_index, must_be_present))
                .collect(),
        })
    }

    /// Visits the start of every line satisfying `plan`, in ascending order.
    fn visit_clause_lines(
        &self,
        haystack: &[u8],
        config: SearchConfig,
        plan: &ClausePlan,
        mut visit: impl FnMut(usize),
    ) {
        // One whole-haystack anchor search keeps the matcher's scanning state warm across hits.
        let anchor_matches =
            self.term_checkers[plan.anchor_term_index].search(haystack, config, usize::MAX);
        // `next_line_start` skips further hits on a line that was already resolved.
        let mut next_line_start = 0_usize;
        for matched in anchor_matches {
            if matched.start < next_line_start {
                continue;
            }
            let line_start = memrchr(b'\n', &haystack[next_line_start..matched.start])
                .map_or(next_line_start, |index| next_line_start + index + 1);
            let line_end = line_end_from(haystack, matched.start);
            let content_end = line_content_end(haystack, line_start, line_end);
            let line = &haystack[line_start..content_end];

            if plan.checks.iter().all(|&(term_index, must_be_present)| {
                self.term_checkers[term_index].contains_line_term(line, config) == must_be_present
            }) {
                visit(line_start);
            }
            next_line_start = line_end + 1;
        }
    }
}

/// Returns evenly spaced sample windows spanning a haystack of at least
/// `PLANNER_SAMPLE_WINDOWS * PLANNER_SAMPLE_WINDOW_LEN` bytes.
fn sample_windows(haystack: &[u8]) -> [&[u8]; PLANNER_SAMPLE_WINDOWS] {
    let max_start = haystack.len() - PLANNER_SAMPLE_WINDOW_LEN;
    std::array::from_fn(|window_index| {
        let start = max_start * window_index / (PLANNER_SAMPLE_WINDOWS - 1);
        &haystack[start..start + PLANNER_SAMPLE_WINDOW_LEN]
    })
}
