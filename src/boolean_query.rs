use std::cmp::Reverse;
use std::collections::HashMap;

use grep_regex::RegexMatcher;
use smallvec::SmallVec;

use crate::exact::{MultiPatternEngine, SinglePatternEngine};
use crate::{Match, SearchConfig, SearchError, build_line_matcher, normalize_pattern};

pub(crate) struct BooleanQueryEngine {
    patterns: Vec<CompiledBooleanPattern>,
    term_lengths: Vec<usize>,
    // A single term reuses its checker rather than building a second identical engine.
    term_engine: Option<MultiPatternEngine>,
    /// The term engine's configuration. Terms that can share bytes, like `cat` and
    /// `category`, need overlapping search, or one would hide the other.
    term_config: SearchConfig,
    line_matcher: RegexMatcher,
    term_checkers: Vec<SinglePatternEngine>,
    can_match_without_terms: bool,
    line_count_clauses: Option<Vec<BooleanClause>>,
}

/// Whether occurrences of two `terms` can share bytes: one contains the other, or one's
/// suffix is the other's prefix.
fn terms_share_bytes(terms: &[Vec<u8>]) -> bool {
    let shares = |first: &[u8], second: &[u8]| {
        memchr::memmem::find(first, second).is_some()
            || (1..first.len().min(second.len())).any(|shared| first.ends_with(&second[..shared]))
    };
    terms.iter().enumerate().any(|(index, first)| {
        terms
            .iter()
            .enumerate()
            .any(|(other, second)| index != other && shares(first, second))
    })
}

#[derive(Debug)]
struct CompiledBooleanPattern {
    expression: BooleanExpr,
    pattern_index: usize,
    /// By term index, whether the term counts toward a match rather than against one.
    positive_terms: Vec<bool>,
}

#[derive(Debug, Clone)]
enum Expr<T> {
    Literal(T),
    Not(Box<Expr<T>>),
    And(Box<Expr<T>>, Box<Expr<T>>),
    Or(Box<Expr<T>>, Box<Expr<T>>),
}

type BooleanExpr = Expr<usize>;
type ParsedExpr = Expr<String>;

#[derive(Debug, Clone)]
struct BooleanClause {
    positive_terms: SmallVec<[usize; 4]>,
    negative_terms: SmallVec<[usize; 2]>,
}

impl BooleanQueryEngine {
    pub(crate) fn new(patterns: &[Vec<u8>], config: SearchConfig) -> Result<Self, SearchError> {
        let mut compiled_patterns = Vec::with_capacity(patterns.len());
        let mut unique_terms = Vec::new();
        let mut term_indexes = HashMap::<Vec<u8>, usize>::new();

        for (pattern_index, pattern) in patterns.iter().enumerate() {
            let expression_text =
                std::str::from_utf8(pattern).expect("patterns originate from UTF-8 strings");
            let tokens = tokenize(expression_text, pattern_index)?;
            let mut parser = Parser::new(tokens, pattern_index);
            let parsed = parser.parse_expression()?;
            let expression = intern_expression_terms(
                parsed,
                &mut unique_terms,
                &mut term_indexes,
                pattern_index,
            )?;
            compiled_patterns.push(CompiledBooleanPattern {
                expression,
                pattern_index,
                positive_terms: Vec::new(),
            });
        }
        for pattern in &mut compiled_patterns {
            pattern.positive_terms = vec![false; unique_terms.len()];
            pattern
                .expression
                .mark_positive_terms(true, &mut pattern.positive_terms);
        }

        let term_lengths = unique_terms.iter().map(Vec::len).collect::<Vec<_>>();
        let all_terms_absent = vec![false; term_lengths.len()];
        let can_match_without_terms = compiled_patterns
            .iter()
            .any(|pattern| pattern.expression.evaluate(all_terms_absent.as_slice()));
        let line_count_clauses = if can_match_without_terms {
            None
        } else {
            collect_line_count_clauses(compiled_patterns.as_slice())
        };
        let prefilter_terms = collect_prefilter_terms(compiled_patterns.as_slice(), &term_lengths);
        let prefilter_patterns = prefilter_terms
            .iter()
            .map(|&term_index| unique_terms[term_index].clone())
            .collect::<Vec<_>>();
        let line_matcher = build_line_matcher(prefilter_patterns.as_slice(), config)?;
        let folded_terms = unique_terms
            .iter()
            .map(|term| normalize_pattern(term.as_slice(), config.case_mode()))
            .collect::<Vec<_>>();
        // `..config` copies every other field of the configuration.
        let term_config = SearchConfig {
            overlapping: config.overlapping() || terms_share_bytes(&folded_terms),
            ..config
        };
        let term_engine = if unique_terms.len() == 1 {
            None
        } else {
            Some(MultiPatternEngine::new(
                unique_terms.as_slice(),
                term_config,
            )?)
        };
        let term_checkers = folded_terms
            .into_iter()
            .map(SinglePatternEngine::new)
            .collect::<Vec<_>>();

        Ok(Self {
            patterns: compiled_patterns,
            term_lengths,
            term_engine,
            term_config,
            line_matcher,
            term_checkers,
            can_match_without_terms,
            line_count_clauses,
        })
    }

    /// Finds the matches of the lines each query holds on: the occurrences of the query's
    /// positive terms, or the whole line when it holds through `NOT` alone.
    pub(crate) fn search(&self, haystack: &[u8], config: SearchConfig, limit: usize) -> Vec<Match> {
        if limit == 0 || haystack.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::with_capacity(limit.min(64));
        if self.can_match_without_terms {
            self.visit_term_lines(haystack, config, |start, end, present_terms, terms| {
                self.push_line_matches((start, end), 0, terms, present_terms, config, &mut results);
                results.len() < limit
            });
        } else {
            // The prefilter finds the matching lines; only those are searched for their terms.
            let mut present_terms = vec![false; self.term_lengths.len()];
            self.visit_candidate_matches(haystack, config, |line| {
                let terms = self.term_matches(&haystack[line.start..line.end], config);
                for term in &terms {
                    present_terms[term.pattern_index] = true;
                }
                let line_bounds = (line.start, line.end);
                let offset = line.start;
                self.push_line_matches(
                    line_bounds,
                    offset,
                    &terms,
                    &present_terms,
                    config,
                    &mut results,
                );
                for term in &terms {
                    present_terms[term.pattern_index] = false;
                }
                results.len() < limit
            });
        }
        results.truncate(limit);
        results
    }

    /// Adds the matches of the line `bounds`, whose term matches start at `offset`.
    fn push_line_matches(
        &self,
        bounds: (usize, usize),
        offset: usize,
        terms: &[Match],
        present_terms: &[bool],
        config: SearchConfig,
        results: &mut Vec<Match>,
    ) {
        let first = results.len();
        let mut held_by_negation = None;
        for pattern in &self.patterns {
            if !pattern.expression.evaluate(present_terms) {
                continue;
            }
            let before = results.len();
            results.extend(
                (terms.iter())
                    .filter(|term| pattern.positive_terms[term.pattern_index])
                    .map(|term| Match {
                        start: offset + term.start,
                        end: offset + term.end,
                        pattern_index: pattern.pattern_index,
                    }),
            );
            if results.len() == before {
                held_by_negation.get_or_insert(pattern.pattern_index);
            }
        }
        if results.len() == first {
            if let Some(pattern_index) = held_by_negation {
                results.push(Match {
                    start: bounds.0,
                    end: bounds.1,
                    pattern_index,
                });
            }
            return;
        }
        results[first..]
            .sort_unstable_by_key(|found| (found.start, found.pattern_index, Reverse(found.end)));
        if config.overlapping() {
            return;
        }
        // Queries sharing a term, and terms sharing bytes, overlap; the leftmost match wins.
        let mut kept = first;
        for index in first..results.len() {
            if kept == first || results[index].start >= results[kept - 1].end {
                results[kept] = results[index];
                kept += 1;
            }
        }
        results.truncate(kept);
    }

    pub(crate) fn find_first(&self, haystack: &[u8], config: SearchConfig) -> Option<Match> {
        self.search(haystack, config, 1).into_iter().next()
    }

    pub(crate) fn count_matching_lines(&self, haystack: &[u8], config: SearchConfig) -> usize {
        if haystack.is_empty() {
            return 0;
        }

        if self.can_match_without_terms {
            return self.count_lines_from_term_matches(haystack, config);
        }

        if let Some(clauses) = self.line_count_clauses.as_deref() {
            return self.count_clause_lines(haystack, config, clauses);
        }

        let mut count = 0;
        self.visit_candidate_matches(haystack, config, |_| {
            count += 1;
            true
        });
        count
    }
}

mod expression;
mod lines;
mod parser;
mod planner;

pub(crate) use planner::BooleanLinePlan;

use expression::{collect_line_count_clauses, collect_prefilter_terms};
use parser::{Parser, intern_expression_terms, tokenize};

#[cfg(test)]
mod tests {
    use super::terms_share_bytes;

    #[test]
    fn terms_share_bytes_when_one_holds_or_continues_another() {
        let shares = |terms: &[&str]| {
            terms_share_bytes(
                &terms
                    .iter()
                    .map(|term| term.as_bytes().to_vec())
                    .collect::<Vec<_>>(),
            )
        };

        assert!(shares(&["cat", "category"]));
        assert!(shares(&["category", "cat"]));
        // The end of one term starts the other, in either order of the two.
        assert!(shares(&["status", "user", "error"]));
        assert!(shares(&["error", "status", "user"]));
        assert!(!shares(&["error", "status=500"]));
        assert!(!shares(&["cat"]));
        assert!(!shares(&[]));
    }
}
