use smallvec::SmallVec;

use super::{BooleanClause, CompiledBooleanPattern, Expr};

/// Most clauses a query is expanded into for line counting. Each clause is another haystack
/// search, so larger expansions are evaluated as written instead.
const MAX_CLAUSES: usize = 64;

impl<T> Expr<T> {
    pub(super) fn try_map<U, E>(
        self,
        map_literal: &mut impl FnMut(T) -> Result<U, E>,
    ) -> Result<Expr<U>, E> {
        Ok(match self {
            Self::Literal(value) => Expr::Literal(map_literal(value)?),
            Self::Not(expr) => Expr::Not(Box::new((*expr).try_map(map_literal)?)),
            Self::And(left, right) => Expr::And(
                Box::new((*left).try_map(map_literal)?),
                Box::new((*right).try_map(map_literal)?),
            ),
            Self::Or(left, right) => Expr::Or(
                Box::new((*left).try_map(map_literal)?),
                Box::new((*right).try_map(map_literal)?),
            ),
        })
    }
}

impl Expr<usize> {
    /// Marks the terms under an even number of `NOT`s, which count toward a match, when
    /// `positive`; `marks` is indexed by term.
    pub(super) fn mark_positive_terms(&self, positive: bool, marks: &mut [bool]) {
        match self {
            Self::Literal(index) => marks[*index] |= positive,
            Self::Not(expr) => expr.mark_positive_terms(!positive, marks),
            Self::And(left, right) | Self::Or(left, right) => {
                left.mark_positive_terms(positive, marks);
                right.mark_positive_terms(positive, marks);
            }
        }
    }

    pub(super) fn evaluate(&self, present_terms: &[bool]) -> bool {
        self.evaluate_with(&mut |index| present_terms.get(index).copied().unwrap_or(false))
    }

    /// One traversal supports eager presence bits and lazily resolved, cached terms.
    #[inline]
    pub(super) fn evaluate_with(&self, resolve: &mut impl FnMut(usize) -> bool) -> bool {
        match self {
            Self::Literal(index) => resolve(*index),
            Self::Not(expr) => !expr.evaluate_with(resolve),
            Self::And(left, right) => left.evaluate_with(resolve) && right.evaluate_with(resolve),
            Self::Or(left, right) => left.evaluate_with(resolve) || right.evaluate_with(resolve),
        }
    }

    fn clauses(&self) -> Option<Vec<BooleanClause>> {
        match self {
            Self::Literal(index) => {
                let mut positive_terms = SmallVec::new();
                positive_terms.push(*index);
                Some(vec![BooleanClause {
                    positive_terms,
                    negative_terms: SmallVec::new(),
                }])
            }
            Self::Not(expr) => match expr.as_ref() {
                Self::Literal(index) => {
                    let mut negative_terms = SmallVec::new();
                    negative_terms.push(*index);
                    Some(vec![BooleanClause {
                        positive_terms: SmallVec::new(),
                        negative_terms,
                    }])
                }
                Self::Not(_) | Self::And(_, _) | Self::Or(_, _) => None,
            },
            Self::And(left, right) => {
                let left_clauses = left.clauses()?;
                let right_clauses = right.clauses()?;
                // Clauses multiply, so a short query can expand past what fits in memory.
                let clause_count = left_clauses.len() * right_clauses.len();
                if clause_count > MAX_CLAUSES {
                    return None;
                }
                let mut clauses = Vec::with_capacity(clause_count);
                for left_clause in &left_clauses {
                    for right_clause in &right_clauses {
                        let mut positive_terms = left_clause.positive_terms.clone();
                        append_unique(
                            &mut positive_terms,
                            right_clause.positive_terms.iter().copied(),
                        );

                        let mut negative_terms = left_clause.negative_terms.clone();
                        append_unique(
                            &mut negative_terms,
                            right_clause.negative_terms.iter().copied(),
                        );

                        clauses.push(BooleanClause {
                            positive_terms,
                            negative_terms,
                        });
                    }
                }
                Some(clauses)
            }
            Self::Or(left, right) => {
                let mut clauses = left.clauses()?;
                clauses.extend(right.clauses()?);
                (clauses.len() <= MAX_CLAUSES).then_some(clauses)
            }
        }
    }

    fn prefilter_terms(&self, term_lengths: &[usize]) -> Option<Vec<usize>> {
        match self {
            Self::Literal(index) => Some(vec![*index]),
            Self::Not(_) => None,
            Self::And(left, right) => match (
                left.prefilter_terms(term_lengths),
                right.prefilter_terms(term_lengths),
            ) {
                (Some(left_terms), Some(right_terms)) => {
                    let left_weight = prefilter_weight(left_terms.as_slice(), term_lengths);
                    let right_weight = prefilter_weight(right_terms.as_slice(), term_lengths);
                    if left_weight <= right_weight {
                        Some(left_terms)
                    } else {
                        Some(right_terms)
                    }
                }
                (Some(terms), None) | (None, Some(terms)) => Some(terms),
                (None, None) => None,
            },
            Self::Or(left, right) => {
                let mut left_terms = left.prefilter_terms(term_lengths)?;
                append_unique(&mut left_terms, right.prefilter_terms(term_lengths)?);
                Some(left_terms)
            }
        }
    }
}

pub(super) fn collect_line_count_clauses(
    patterns: &[CompiledBooleanPattern],
) -> Option<Vec<BooleanClause>> {
    let mut clauses = Vec::new();
    for pattern in patterns {
        clauses.extend(pattern.expression.clauses()?);
    }
    Some(clauses)
}

pub(super) fn collect_prefilter_terms(
    patterns: &[CompiledBooleanPattern],
    term_lengths: &[usize],
) -> Vec<usize> {
    let mut prefilter_terms = Vec::new();

    for pattern in patterns {
        if let Some(pattern_terms) = pattern.expression.prefilter_terms(term_lengths) {
            append_unique(&mut prefilter_terms, pattern_terms);
        }
    }

    prefilter_terms
}

/// Extends either an inline SmallVec or a Vec, preserving first-occurrence order.
fn append_unique(
    terms: &mut (impl AsRef<[usize]> + Extend<usize>),
    additions: impl IntoIterator<Item = usize>,
) {
    for term in additions {
        if !terms.as_ref().contains(&term) {
            terms.extend(std::iter::once(term));
        }
    }
}

fn prefilter_weight(terms: &[usize], term_lengths: &[usize]) -> usize {
    terms.iter().copied().map(|index| term_lengths[index]).sum()
}
