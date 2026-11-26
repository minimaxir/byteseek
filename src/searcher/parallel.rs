use super::{Engine, Searcher};
use crate::chunks::{concat_chunks, map_chunks, release_unused_capacity};
use crate::{CaseMode, Match, profiles, window_matches_ascii_insensitive};

/// Returns whether no exact pattern occurrence crosses `boundary`.
pub(super) fn boundary_is_safe(
    haystack: &[u8],
    pattern: &[u8],
    boundary: usize,
    case_mode: CaseMode,
) -> bool {
    if boundary == 0 || boundary == haystack.len() {
        return true;
    }

    let first_start = boundary.saturating_sub(pattern.len().saturating_sub(1));
    for start in first_start..boundary {
        let Some(end) = start.checked_add(pattern.len()) else {
            return false;
        };
        if end <= boundary || end > haystack.len() {
            continue;
        }
        let candidate = &haystack[start..end];
        let crosses = match case_mode {
            CaseMode::Sensitive => candidate == pattern,
            CaseMode::AsciiInsensitive => window_matches_ascii_insensitive(candidate, pattern),
        };
        if crosses {
            return false;
        }
    }
    true
}

impl Searcher {
    /// Whether the haystack is large enough for parallel chunk search. The thread count is
    /// checked last because the first check starts rayon's global pool.
    #[inline]
    pub(super) fn should_parallelize(&self, haystack_len: usize) -> bool {
        let threshold = match &self.engine {
            Engine::Single(_) => profiles::PARALLEL_SINGLE_THRESHOLD,
            Engine::Multi(_) => profiles::PARALLEL_MULTI_THRESHOLD,
            Engine::Approximate(_) | Engine::Boolean(_) | Engine::Structured(_) => usize::MAX,
        };
        haystack_len >= threshold && profiles::has_parallel_workers()
    }

    /// Number of chunks for exact parallel search. Several per thread let slower threads take
    /// fewer chunks.
    pub(super) fn exact_chunk_count(&self, haystack_len: usize) -> usize {
        let chunks_per_thread = match &self.engine {
            Engine::Single(_) | Engine::Multi(_) | Engine::Boolean(_) => 4,
            Engine::Approximate(_) | Engine::Structured(_) => 1,
        };
        (chunks_per_thread * rayon::current_num_threads())
            .min(haystack_len / profiles::MIN_CHUNK_SIZE)
            .max(2)
    }

    /// Searches overlapping chunks in parallel and merges up to `limit` matches in order.
    /// Each match is kept only by the chunk it starts in.
    pub(super) fn find_parallel_chunks(
        &self,
        haystack: &[u8],
        limit: usize,
        search: impl Fn(&[u8]) -> Vec<Match> + Sync,
    ) -> Vec<Match> {
        let overlap = self.max_pattern_len.saturating_sub(1);
        // Several chunks per worker let faster cores take more of them.
        const CHUNKS_PER_WORKER: usize = 4;

        let num_chunks = (CHUNKS_PER_WORKER * rayon::current_num_threads())
            .min(haystack.len() / profiles::MIN_CHUNK_SIZE)
            .max(2);
        let base_chunk_size = haystack.len() / num_chunks;

        concat_chunks(num_chunks, Some(limit), |chunk_index| {
            let start = chunk_index * base_chunk_size;
            let is_last = chunk_index == num_chunks - 1;
            let end = if is_last {
                haystack.len()
            } else {
                ((chunk_index + 1) * base_chunk_size)
                    .saturating_add(overlap)
                    .min(haystack.len())
            };
            let chunk = &haystack[start..end];
            // Matches starting at or after `boundary` belong to the next chunk.
            let boundary = if is_last {
                chunk.len()
            } else {
                (chunk_index + 1) * base_chunk_size - start
            };

            let mut local_matches = search(chunk);

            local_matches.retain(|matched| matched.start < boundary);
            for matched in &mut local_matches {
                matched.start += start;
                matched.end += start;
            }
            release_unused_capacity(&mut local_matches);
            local_matches
        })
    }

    /// Counts up to `limit` exact matches without collecting them, splitting the input like
    /// [`Searcher::find_bytes_with_limit`] so the counts agree. Returns `None` if the engine
    /// can only count collected matches.
    pub(super) fn count_exact(&self, haystack: &[u8], limit: usize) -> Option<usize> {
        // Plan once from the whole haystack, as `find_bytes_with_limit` does.
        let planned = self.block_plan(haystack);
        let count_sequential = |chunk: &[u8]| match &self.engine {
            Engine::Single(engine) => {
                Some(engine.count_planned(chunk, self.config, limit, planned))
            }
            Engine::Multi(engine) => engine.count_planned(chunk, self.config, limit, planned),
            Engine::Approximate(_) | Engine::Boolean(_) | Engine::Structured(_) => None,
        };
        if !matches!(self.engine, Engine::Single(_) | Engine::Multi(_)) {
            return None;
        }
        // Overlapping chunks would double count, so overlapping search counts in one piece.
        if !self.should_parallelize(haystack.len()) || self.config.whole_word() {
            return count_sequential(haystack);
        }
        if self.config.overlapping() {
            return count_sequential(haystack);
        }
        let Some(splits) = self.exact_split_points(haystack) else {
            return count_sequential(haystack);
        };
        map_chunks(splits.len() - 1, |chunk| {
            count_sequential(&haystack[splits[chunk]..splits[chunk + 1]])
        })
        .into_iter()
        .sum::<Option<usize>>()
        .map(|count| count.min(limit))
    }

    pub(super) fn exact_split_points(&self, haystack: &[u8]) -> Option<Vec<usize>> {
        const MAX_SPLIT_PATTERN_LEN: usize = 256;

        if self.max_pattern_len > MAX_SPLIT_PATTERN_LEN {
            return None;
        }

        let num_chunks = self.exact_chunk_count(haystack.len());
        let base_chunk_size = haystack.len() / num_chunks;
        let mut splits = Vec::with_capacity(num_chunks + 1);
        splits.push(0_usize);

        for chunk_index in 1..num_chunks {
            let target = chunk_index * base_chunk_size;
            let mut safe_split = None;
            for distance in 0..=self.max_pattern_len {
                for candidate in [target.checked_add(distance), target.checked_sub(distance)]
                    .into_iter()
                    .flatten()
                {
                    if candidate <= *splits.last().unwrap_or(&0) || candidate >= haystack.len() {
                        continue;
                    }
                    let is_safe = match &self.engine {
                        Engine::Single(engine) => boundary_is_safe(
                            haystack,
                            engine.pattern_bytes(),
                            candidate,
                            self.config.case_mode(),
                        ),
                        Engine::Multi(engine) => {
                            engine.boundary_is_safe(haystack, candidate, self.max_pattern_len)
                        }
                        Engine::Approximate(_) | Engine::Boolean(_) | Engine::Structured(_) => {
                            false
                        }
                    };
                    if is_safe {
                        safe_split = Some(candidate);
                        break;
                    }
                }
                if safe_split.is_some() {
                    break;
                }
            }
            splits.push(safe_split?);
        }

        splits.push(haystack.len());
        Some(splits)
    }
}

pub(super) fn search_disjoint_chunks(
    haystack: &[u8],
    splits: &[usize],
    limit: usize,
    search: impl Fn(&[u8]) -> Vec<Match> + Sync,
) -> Vec<Match> {
    concat_chunks(splits.len() - 1, Some(limit), |chunk| {
        let start = splits[chunk];
        let mut matches = search(&haystack[start..splits[chunk + 1]]);
        for matched in &mut matches {
            matched.start += start;
            matched.end += start;
        }
        release_unused_capacity(&mut matches);
        matches
    })
}

pub(super) fn line_split_points(haystack: &[u8], num_chunks: usize) -> Vec<usize> {
    let chunk_size = haystack.len() / num_chunks;
    let mut splits = Vec::with_capacity(num_chunks + 1);
    splits.push(0);
    for chunk_index in 1..num_chunks {
        let target = chunk_index * chunk_size;
        let split = memchr::memchr(b'\n', &haystack[target..])
            .map_or(haystack.len(), |offset| target + offset + 1);
        if split < haystack.len() && split != *splits.last().unwrap_or(&0) {
            splits.push(split);
        }
    }
    splits.push(haystack.len());
    splits
}
