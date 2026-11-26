mod literal;
mod multi;
mod pairs;
mod single;

pub(crate) use literal::{BlockPlan, LiteralScanner, STOP};
pub(crate) use multi::{LineCountPlan, MultiPatternEngine};
pub(crate) use pairs::{
    BLOCK, Extent, Literal, PairPlan, PairSample, PairScanner, adjacent_blocks, flagged_positions,
    for_each_mark, marks_differ_folded,
};
pub(crate) use single::SinglePatternEngine;

/// Keeps native collection in a compact kernel, separate from visitor/strategy dispatch.
#[inline(never)]
fn collect_up_to(
    matches: impl Iterator<Item = crate::Match>,
    capacity: usize,
    limit: usize,
) -> Vec<crate::Match> {
    let mut output = Vec::with_capacity(capacity);
    if limit == usize::MAX {
        output.extend(matches);
    } else {
        output.extend(matches.take(limit));
    }
    output
}

/// Counts native iterator hits, keeping the unlimited hot loop free of limit bookkeeping.
#[inline(always)]
fn count_up_to(matches: impl Iterator, limit: usize) -> usize {
    if limit == usize::MAX {
        matches.count()
    } else {
        matches.take(limit).count()
    }
}
