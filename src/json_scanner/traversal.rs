use super::bytes::{container_end_within, skip_container, skip_value, skip_ws};
use std::marker::PhantomData;

use super::collection::{HitPath, push_hit};
use super::parallel::{FoldSplitScan, PARALLEL_SCAN_LEN, split_scan};
use super::{JsonScalarHit, object_member};
use crate::jq_path::{append_array_segment, append_json_key_segment};
use crate::occurrences::{Needle, OccurrenceCursor};

/// Traversal callbacks: `visit` sees each scalar, `prune` may skip a container.
pub(super) struct TraversalHooks<'a, S, V, P> {
    pub(super) state: &'a mut S,
    pub(super) visit: V,
    pub(super) prune: P,
}

/// Configuration shared by sequential and parallel prefiltered scans.
pub(super) struct PrefilteredSplitScan<'a, F, P> {
    pub(super) bytes: &'a [u8],
    pub(super) needle: &'a Needle,
    pub(super) min_depth: usize,
    pub(super) matcher: &'a F,
    /// `fn() -> P` keeps the struct `Send + Sync` regardless of `P`.
    pub(super) paths: PhantomData<fn() -> P>,
}

/// The hits of one prefiltered scan, and the occurrence cursor that prunes its containers.
#[derive(Debug)]
pub(super) struct PrefilteredState<'a, P> {
    pub(super) hits: Vec<JsonScalarHit<P>>,
    occurrences: OccurrenceCursor<'a>,
}

impl<'a, F: Fn(&[u8]) -> bool, P: HitPath> PrefilteredSplitScan<'a, F, P> {
    pub(super) fn new_scan_state(&self) -> PrefilteredState<'a, P> {
        PrefilteredState {
            hits: Vec::new(),
            occurrences: OccurrenceCursor::new(self.needle, self.bytes),
        }
    }

    /// Scans the value at `pos`, skipping containers without a needle occurrence, and returns
    /// the position after it. `LIMITED` scans stop (returning the input length) at `limit` hits.
    #[inline]
    pub(super) fn scan_into<const LIMITED: bool>(
        &self,
        state: &mut PrefilteredState<'a, P>,
        (pos, depth): (usize, usize),
        path: &mut Vec<u8>,
        limit: usize,
    ) -> usize {
        let PrefilteredState { hits, occurrences } = state;
        let mut hooks = TraversalHooks {
            state: hits,
            visit: |hits: &mut Vec<JsonScalarHit<P>>, path: &[u8], start, end| {
                record_hit(self.bytes, path, (start, end), self.matcher, hits);

                true
            },
            prune: |hits: &Vec<JsonScalarHit<P>>, pos, depth, kind| {
                if LIMITED && hits.len() >= limit {
                    return Some(self.bytes.len());
                }
                // Too-deep containers are skipped by the walker's depth guard instead.
                if depth < self.min_depth || depth >= MAX_SCAN_DEPTH {
                    return None;
                }
                let close = match kind {
                    Some(b'{') => b'}',
                    Some(b'[') => b']',
                    _ => return None,
                };
                let brackets = (self.bytes[pos], close);
                occurrences.skippable_end(pos, |limit| {
                    container_end_within(self.bytes, pos, brackets, limit)
                })
            },
        };
        // Hits that keep no path leave it unbuilt.
        if P::KEEPS_PATH {
            scan_value_recursive::<true, true, _, _, _>(self.bytes, pos, depth, path, &mut hooks)
        } else {
            scan_value_recursive::<true, false, _, _, _>(self.bytes, pos, depth, path, &mut hooks)
        }
    }
}

#[inline]
fn record_hit<F: Fn(&[u8]) -> bool, P: HitPath>(
    bytes: &[u8],
    path: &[u8],
    (start, end): (usize, usize),
    matcher: &F,
    hits: &mut Vec<JsonScalarHit<P>>,
) {
    let value = if bytes[start] == b'"' {
        &bytes[start + 1..end - 1]
    } else {
        &bytes[start..end]
    };
    if matcher(value) {
        push_hit(hits, path, start, end);
    }
}

/// Returns up to `limit` scalar values for which `matcher` returns true.
pub(crate) fn json_scan_values<F, P>(
    bytes: &[u8],
    limit: usize,
    matcher: F,
) -> Vec<JsonScalarHit<P>>
where
    F: Fn(&[u8]) -> bool + Sync,
    P: HitPath,
{
    let record = |hits: &mut Vec<JsonScalarHit<P>>, path: &[u8], start: usize, end: usize| {
        record_hit(bytes, path, (start, end), &matcher, hits);
    };
    if limit == usize::MAX {
        return json_fold_values_with_paths(bytes, Vec::new, record)
            .into_iter()
            .flatten()
            .collect();
    }

    let mut hits = Vec::new();
    if limit == 0 {
        return hits;
    }
    super::json_scan_values_with_paths(bytes, |path, start, end| {
        record(&mut hits, path, start, end);
        hits.len() < limit
    });
    hits
}

/// Visits every scalar span with its path. Returning false from `visit` stops traversal.
pub(crate) fn json_scan_values_with_paths(
    bytes: &[u8],
    mut visit: impl FnMut(&[u8], usize, usize) -> bool,
) {
    let mut path = Vec::with_capacity(128);
    path.push(b'$');
    let mut hooks = TraversalHooks {
        state: &mut (),
        visit: |_: &mut (), path: &[u8], start, end| visit(path, start, end),
        prune: never_prune,
    };
    scan_value_recursive::<false, true, _, _, _>(
        bytes,
        skip_ws(bytes, 0),
        0,
        &mut path,
        &mut hooks,
    );
}

/// Like [`json_scan_values`], but skips containers that do not contain `needle`.
///
/// The prefilter applies only from `prefilter_min_depth` down, since checking top-level
/// containers that span most of the document costs more than it saves.
pub(crate) fn json_scan_values_prefiltered<F, P>(
    bytes: &[u8],
    needle: &Needle,
    prefilter_min_depth: usize,
    limit: usize,
    matcher: F,
) -> Vec<JsonScalarHit<P>>
where
    F: Fn(&[u8]) -> bool + Sync,
    P: HitPath,
{
    let scan = PrefilteredSplitScan {
        bytes,
        needle,
        min_depth: prefilter_min_depth,
        matcher: &matcher,
        paths: PhantomData,
    };
    if limit == usize::MAX && bytes.len() >= PARALLEL_SCAN_LEN {
        return split_scan(bytes, &scan)
            .into_iter()
            .flat_map(|state| state.hits)
            .collect();
    }
    let mut state = scan.new_scan_state();
    let mut path = Vec::with_capacity(128);
    path.push(b'$');
    let start = (skip_ws(bytes, 0), 0);
    if limit == usize::MAX {
        scan.scan_into::<false>(&mut state, start, &mut path, limit);
    } else {
        scan.scan_into::<true>(&mut state, start, &mut path, limit);
    }
    state.hits
}

/// Folds every scalar into `init()` states, returned in document order. Large top-level arrays
/// are split into parallel groups that each fold into their own state.
pub(crate) fn json_fold_values_with_paths<S, I, V>(bytes: &[u8], init: I, visit: V) -> Vec<S>
where
    S: Send,
    I: Fn() -> S + Sync,
    V: Fn(&mut S, &[u8], usize, usize) + Sync,
{
    if bytes.len() >= PARALLEL_SCAN_LEN {
        return split_scan(bytes, &FoldSplitScan { bytes, init, visit });
    }
    let mut state = init();
    fold_document(bytes, &mut Vec::new(), &mut state, &visit);
    vec![state]
}

/// Folds every scalar of a document too small for [`json_fold_values_with_paths`] to split
/// into `state`, on this thread. Paths are built in `path`, whose storage a caller folding
/// many documents reuses.
///
/// # Returns
///
/// Whether the document was folded: a larger one is left for
/// [`json_fold_values_with_paths`].
pub(crate) fn json_fold_small_document<S>(
    bytes: &[u8],
    path: &mut Vec<u8>,
    state: &mut S,
    visit: &impl Fn(&mut S, &[u8], usize, usize),
) -> bool {
    if bytes.len() >= PARALLEL_SCAN_LEN {
        return false;
    }
    fold_document(bytes, path, state, visit);
    true
}

/// Folds every scalar of the document `bytes` into `state` on this thread, building paths in
/// `path`.
fn fold_document<S>(
    bytes: &[u8],
    path: &mut Vec<u8>,
    state: &mut S,
    visit: &impl Fn(&mut S, &[u8], usize, usize),
) {
    path.clear();
    path.push(b'$');
    let mut hooks = TraversalHooks {
        state,
        visit: fold_visit(visit),
        prune: never_prune,
    };
    scan_value_recursive::<false, true, _, _, _>(bytes, skip_ws(bytes, 0), 0, path, &mut hooks);
}

/// Returns a visiting hook that folds every scalar with `visit` and never stops the walk.
pub(super) fn fold_visit<S>(
    visit: &impl Fn(&mut S, &[u8], usize, usize),
) -> impl FnMut(&mut S, &[u8], usize, usize) -> bool {
    move |state, path, start, end| {
        visit(state, path, start, end);
        true
    }
}

/// A pruning hook for walks that skip no container.
pub(super) fn never_prune<S>(_: &S, _: usize, _: usize, _: Option<u8>) -> Option<usize> {
    None
}

/// Containers nested deeper than this are skipped, bounding recursion on adversarial input.
const MAX_SCAN_DEPTH: usize = 1024;

/// The recursive walker. `PREFILTERED = false` compiles the pruning hook away, and
/// `PATHS = false` the building of `path`, for visits that read none.
pub(super) fn scan_value_recursive<const PREFILTERED: bool, const PATHS: bool, S, V, P>(
    bytes: &[u8],
    pos: usize,
    depth: usize,
    path: &mut Vec<u8>,
    hooks: &mut TraversalHooks<'_, S, V, P>,
) -> usize
where
    V: FnMut(&mut S, &[u8], usize, usize) -> bool,
    P: FnMut(&S, usize, usize, Option<u8>) -> Option<usize>,
{
    let kind = bytes.get(pos).copied();
    if PREFILTERED && let Some(end) = (hooks.prune)(hooks.state, pos, depth, kind) {
        return end;
    }
    let Some(open) = kind else {
        return pos;
    };
    match open {
        b'{' | b'[' if depth >= MAX_SCAN_DEPTH => {
            skip_container(bytes, pos, open, if open == b'{' { b'}' } else { b']' })
        }
        b'{' => {
            scan_children::<PREFILTERED, PATHS, true, _, _, _>(bytes, (pos, depth), path, hooks)
        }
        b'[' => {
            scan_children::<PREFILTERED, PATHS, false, _, _, _>(bytes, (pos, depth), path, hooks)
        }
        _ => {
            let end = skip_value(bytes, pos);
            if (hooks.visit)(hooks.state, path, pos, end) {
                end
            } else {
                bytes.len()
            }
        }
    }
}

/// Walks a container's children; `OBJECT` is const so the loop has no object/array branch.
#[inline]
fn scan_children<const PREFILTERED: bool, const PATHS: bool, const OBJECT: bool, S, V, P>(
    bytes: &[u8],
    (pos, depth): (usize, usize),
    path: &mut Vec<u8>,
    hooks: &mut TraversalHooks<'_, S, V, P>,
) -> usize
where
    V: FnMut(&mut S, &[u8], usize, usize) -> bool,
    P: FnMut(&S, usize, usize, Option<u8>) -> Option<usize>,
{
    let (open, close) = if OBJECT { (b'{', b'}') } else { (b'[', b']') };
    let mut position = skip_ws(bytes, pos + 1);
    if bytes.get(position) == Some(&close) {
        return position + 1;
    }
    let mut index = 0;
    loop {
        let path_len = path.len();
        if OBJECT {
            let Some((start, end, value)) = object_member(bytes, position) else {
                return skip_container(bytes, pos, open, close);
            };
            if PATHS {
                append_json_key_segment(path, bytes, start, end);
            }
            position = value;
        } else {
            if position >= bytes.len() {
                return position;
            }
            if PATHS {
                append_array_segment(path, index);
                index += 1;
            }
        }
        position = scan_value_recursive::<PREFILTERED, PATHS, _, _, _>(
            bytes,
            position,
            depth + 1,
            path,
            hooks,
        );
        if PATHS {
            path.truncate(path_len);
        }
        position = skip_ws(bytes, position);
        match bytes.get(position) {
            Some(b',') => position = skip_ws(bytes, position + 1),
            Some(&byte) if byte == close => return position + 1,
            _ => return position,
        }
    }
}
