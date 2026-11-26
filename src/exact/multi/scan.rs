use aho_corasick::{Match as AhoMatch, packed};
use smallvec::SmallVec;

use super::{
    FOLDED_CHUNK_LEN, MultiPatternEngine, candidate_match, candidate_match_at, next_line_start,
};
use crate::{CaseMode, Match, SearchConfig, fold_ascii, fold_ascii_bytes};

/// Converts native hits with haystack `offset` and appends at most `limit` total matches.
/// Callers keep `output.len() <= limit`; inlining removes offset arithmetic when it is zero.
#[inline(always)]
pub(super) fn extend_up_to(
    output: &mut Vec<Match>,
    matches: impl Iterator<Item = AhoMatch>,
    offset: usize,
    limit: usize,
) {
    let matches = matches.map(|candidate| candidate_match_at(candidate, offset));
    if limit == usize::MAX {
        output.extend(matches);
    } else {
        output.extend(matches.take(limit - output.len()));
    }
}

pub(super) fn packed_eligible(patterns: &[Vec<u8>], search_config: SearchConfig) -> bool {
    if search_config.overlapping() || patterns.len() > 128 {
        return false;
    }
    // Folded search uses line-aligned chunks, so matches must not cross lines.
    if search_config.case_mode() == CaseMode::AsciiInsensitive
        && patterns.iter().any(|pattern| pattern.contains(&b'\n'))
    {
        return false;
    }

    patterns
        .iter()
        .try_fold(0_usize, |total, pattern| total.checked_add(pattern.len()))
        .is_some_and(|total| total <= 4_096)
}

pub(super) fn build_packed_searcher(
    patterns: &[Vec<u8>],
    search_config: SearchConfig,
) -> Option<Box<packed::Searcher>> {
    if !packed_eligible(patterns, search_config) {
        return None;
    }

    let mut packed_config = packed::Config::new();
    packed_config.match_kind(packed::MatchKind::LeftmostLongest);
    let mut builder = packed_config.builder();
    match search_config.case_mode() {
        CaseMode::Sensitive => {
            builder.extend(patterns.iter().map(Vec::as_slice));
        }
        CaseMode::AsciiInsensitive => {
            for pattern in patterns {
                builder.add(fold_ascii_bytes(pattern));
            }
        }
    }
    builder.build().map(Box::new)
}

impl MultiPatternEngine {
    /// Calls `scan(offset, piece)` on the haystack as packed search must see it: whole, or as
    /// folded line-aligned chunks when `fold_haystack`. `scan` returns `false` to stop.
    pub(super) fn for_each_packed_chunk(
        &self,
        haystack: &[u8],
        mut scan: impl FnMut(usize, &[u8]) -> bool,
    ) {
        if !self.fold_haystack {
            scan(0, haystack);
            return;
        }

        let mut folded = Vec::with_capacity(FOLDED_CHUNK_LEN.min(haystack.len()) + 128);
        let mut chunk_start = 0_usize;
        while chunk_start < haystack.len() {
            let target = chunk_start.saturating_add(FOLDED_CHUNK_LEN);
            let chunk_end = if target >= haystack.len() {
                haystack.len()
            } else {
                next_line_start(haystack, target)
            };
            folded.clear();
            folded.extend(
                haystack[chunk_start..chunk_end]
                    .iter()
                    .copied()
                    .map(fold_ascii),
            );
            if !scan(chunk_start, &folded) {
                return;
            }
            chunk_start = chunk_end;
        }
    }

    /// Visits equal-end groups in automaton order without changing per-mode tie selection.
    #[inline]
    pub(super) fn visit_groups(
        &self,
        haystack: &[u8],
        mut visit: impl FnMut(&mut SmallVec<[Match; 4]>) -> bool,
    ) {
        let mut pending_end = usize::MAX;
        let mut pending = SmallVec::new();
        for candidate in self.standard().find_overlapping_iter(haystack) {
            if candidate.end() != pending_end && !pending.is_empty() {
                if !visit(&mut pending) {
                    return;
                }
                pending.clear();
            }
            pending_end = candidate.end();
            pending.push(candidate_match(candidate));
        }
        if !pending.is_empty() {
            visit(&mut pending);
        }
    }
}
