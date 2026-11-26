use memchr::memmem;

use super::bytes::{container_end_within, skip_value, skip_ws};
use super::collection::HitPath;
pub(super) use super::traversal::{PrefilteredSplitScan, PrefilteredState};
use super::traversal::{TraversalHooks, fold_visit, never_prune, scan_value_recursive};
use super::{member_header, next_item, object_member};
use crate::chunks::map_chunks;
use crate::jq_path::{append_array_segment, append_json_key_segment};
use crate::profiles;

/// Smallest array span scanned in parallel; below it speculation costs more than it saves.
pub(crate) const PARALLEL_SCAN_LEN: usize = 128 * 1024;
/// Smallest group of array elements handed to one parallel scan task.
const MIN_GROUP_LEN: usize = 16 * 1024;
/// Element groups per rayon thread, so work stealing can balance uneven groups.
const GROUPS_PER_THREAD: usize = 4;

/// A recursive scalar scan whose results can be split across array elements.
pub(super) trait SplitScan: Sync {
    type State: Send;

    /// Whether the scan reads paths; if not, array elements are neither counted nor added
    /// to the path.
    const READS_PATHS: bool = true;

    fn new_state(&self) -> Self::State;

    /// Scans the value at `pos` into `state` and returns the position after it.
    fn scan(&self, state: &mut Self::State, pos: usize, depth: usize, path: &mut Vec<u8>) -> usize;

    /// Whether the scan would skip the whole container at `pos` (ending at `end`, if known).
    fn skips_container(&self, _pos: usize, _end: Option<usize>, _depth: usize) -> bool {
        false
    }
}

pub(super) struct FoldSplitScan<'a, I, V> {
    pub(super) bytes: &'a [u8],
    pub(super) init: I,
    pub(super) visit: V,
}

impl<S, I, V> SplitScan for FoldSplitScan<'_, I, V>
where
    S: Send,
    I: Fn() -> S + Sync,
    V: Fn(&mut S, &[u8], usize, usize) + Sync,
{
    type State = S;

    fn new_state(&self) -> S {
        (self.init)()
    }

    fn scan(&self, state: &mut S, pos: usize, depth: usize, path: &mut Vec<u8>) -> usize {
        let mut hooks = TraversalHooks {
            state,
            visit: fold_visit(&self.visit),
            prune: never_prune,
        };
        scan_value_recursive::<false, true, _, _, _>(self.bytes, pos, depth, path, &mut hooks)
    }
}

impl<'a, F, P> SplitScan for PrefilteredSplitScan<'a, F, P>
where
    F: Fn(&[u8]) -> bool + Sync,
    P: HitPath,
{
    type State = PrefilteredState<'a, P>;

    const READS_PATHS: bool = P::KEEPS_PATH;

    fn new_state(&self) -> PrefilteredState<'a, P> {
        self.new_scan_state()
    }

    fn scan(
        &self,
        state: &mut PrefilteredState<'a, P>,
        pos: usize,
        depth: usize,
        path: &mut Vec<u8>,
    ) -> usize {
        self.scan_into::<false>(state, (pos, depth), path, usize::MAX)
    }

    /// Without a known end, only scans up to the first prefilter hit: the container holds a hit
    /// exactly when it is still open after that hit's last byte.
    fn skips_container(&self, pos: usize, end: Option<usize>, depth: usize) -> bool {
        if depth < self.min_depth {
            return false;
        }
        if let Some(end) = end {
            return self.needle.find(&self.bytes[pos..end]).is_none();
        }
        let Some(offset) = self.needle.find(&self.bytes[pos..]) else {
            return true;
        };
        let hit_end = pos + offset + self.needle.as_bytes().len();
        let (open, close) = match self.bytes[pos] {
            b'[' => (b'[', b']'),
            _ => (b'{', b'}'),
        };
        // An empty needle hits at `pos` itself, inside every container.
        hit_end > pos && container_end_within(self.bytes, pos, (open, close), hit_end - 1).is_some()
    }
}

/// Runs `scan` over the whole document, splitting large arrays at the root or directly under
/// a root object across parallel element groups. Returns the states in document order.
pub(super) fn split_scan<S: SplitScan>(bytes: &[u8], scan: &S) -> Vec<S::State> {
    let mut states = vec![scan.new_state()];
    let mut path = Vec::with_capacity(128);
    path.push(b'$');
    let pos = skip_ws(bytes, 0);
    match bytes.get(pos) {
        Some(b'[') => {
            scan_array_split(bytes, scan, pos, 0, &mut path, &mut states);
        }
        Some(b'{') if !scan.skips_container(pos, None, 0) => {
            // Mirrors the sequential object loop, including stopping at a malformed member.
            let mut i = skip_ws(bytes, pos + 1);
            while let Some((key_start, key_end, value)) = object_member(bytes, i) {
                i = value;
                let path_len = path.len();
                append_json_key_segment(&mut path, bytes, key_start, key_end);
                i = if bytes.get(i) == Some(&b'[') {
                    scan_array_split(bytes, scan, i, 1, &mut path, &mut states)
                } else {
                    let state = states.last_mut().expect("states start non-empty");
                    scan.scan(state, i, 1, &mut path)
                };
                path.truncate(path_len);
                let Some(next) = next_item(bytes, i) else {
                    break;
                };
                i = next;
            }
        }
        Some(b'{') => {}
        _ => {
            let state = states.last_mut().expect("states start non-empty");
            scan.scan(state, pos, 0, &mut path);
        }
    }
    states
}

/// Scans the array at `pos` and returns the position after it. A large array of objects is
/// scanned in parallel groups whose states are appended to `states`, followed by a fresh state
/// for what comes after; otherwise it is scanned into the last state.
///
/// Groups start at speculated element starts (see [`ElementShape`]) with speculated indexes.
/// A group is kept only if the previous group ended exactly at its start after the expected
/// number of elements. From the first group that breaks this chain, the rest of the array is
/// rescanned sequentially.
fn scan_array_split<S: SplitScan>(
    bytes: &[u8],
    scan: &S,
    pos: usize,
    depth: usize,
    path: &mut Vec<u8>,
    states: &mut Vec<S::State>,
) -> usize {
    let sequential = |states: &mut Vec<S::State>, path: &mut Vec<u8>| {
        let state = states.last_mut().expect("states start non-empty");
        scan.scan(state, pos, depth, path)
    };
    let first = skip_ws(bytes, pos + 1);
    let Some((starts, shape)) = likely_element_starts(bytes, first) else {
        return sequential(states, path);
    };

    let counts = if S::READS_PATHS {
        map_chunks(starts.len() - 1, |group| {
            1 + shape
                .starts_between(bytes, starts[group], starts[group + 1])
                .count()
        })
    } else {
        Vec::new()
    };
    let first_indices = std::iter::once(0)
        .chain(counts.iter().scan(0, |total, count| {
            *total += count;
            Some(*total)
        }))
        .collect::<Vec<_>>();
    let first_index = |group: usize| first_indices.get(group).copied().unwrap_or(0);
    let elements = ArrayElements {
        bytes,
        scan,
        depth,
        path,
    };
    let groups = map_chunks(starts.len(), |group| {
        let stop = starts.get(group + 1).copied().unwrap_or(usize::MAX);
        let mut state = scan.new_state();
        let (element_count, end) =
            elements.scan_from(&mut state, (starts[group], first_index(group)), stop);
        (state, element_count, end)
    });

    let mut scanned = Vec::with_capacity(groups.len() + 1);
    let mut array_end = None;
    for (group, (state, element_count, end)) in groups.into_iter().enumerate() {
        scanned.push(state);
        match end {
            GroupEnd::Landed(next)
                if starts.get(group + 1) == Some(&next)
                    && counts
                        .get(group)
                        .is_none_or(|&count| count == element_count) => {}
            GroupEnd::Landed(next) => {
                let mut rest = scan.new_state();
                let resumed = (next, first_index(group) + element_count);
                if let (_, GroupEnd::ArrayEnd(end)) =
                    elements.scan_from(&mut rest, resumed, usize::MAX)
                {
                    scanned.push(rest);
                    array_end = Some(end);
                }
                break;
            }
            GroupEnd::ArrayEnd(end) => {
                array_end = Some(end);
                break;
            }
            GroupEnd::Malformed => break,
        }
    }
    // Malformed input is left to the sequential scan.
    let Some(array_end) = array_end else {
        return sequential(states, path);
    };
    if !scan.skips_container(pos, Some(array_end), depth) {
        states.extend(scanned);
        states.push(scan.new_state());
    }
    array_end
}

/// One array's elements, for scanning in runs.
struct ArrayElements<'a, S> {
    bytes: &'a [u8],
    scan: &'a S,
    depth: usize,
    path: &'a [u8],
}

impl<S: SplitScan> ArrayElements<'_, S> {
    /// Scans elements from `start` (with index `index`) until the next one starts at or past
    /// `stop` or the array ends. Returns the number of elements scanned and how the run ended.
    fn scan_from(
        &self,
        state: &mut S::State,
        (start, index): (usize, usize),
        stop: usize,
    ) -> (usize, GroupEnd) {
        let bytes = self.bytes;
        let mut element_path = self.path.to_vec();
        let mut element = start;
        let mut element_index = index;
        let end = loop {
            if S::READS_PATHS {
                append_array_segment(&mut element_path, element_index);
            }
            let element_end = self
                .scan
                .scan(state, element, self.depth + 1, &mut element_path);
            element_path.truncate(self.path.len());
            element_index += 1;
            let separator = skip_ws(bytes, element_end);
            match bytes.get(separator) {
                Some(b',') => {
                    element = skip_ws(bytes, separator + 1);
                    if element >= stop {
                        break GroupEnd::Landed(element);
                    }
                }
                Some(b']') => break GroupEnd::ArrayEnd(separator + 1),
                _ => break GroupEnd::Malformed,
            }
        };
        (element_index - index, end)
    }
}

/// How a run of consecutive array elements ended.
enum GroupEnd {
    /// The next element starts here, at or past the stop.
    Landed(usize),
    /// The position after the array's `]`.
    ArrayEnd(usize),
    /// A separator other than `,` or `]` followed an element.
    Malformed,
}

/// Returns evenly spread likely element starts (beginning with `first`) and the element shape,
/// or `None` when the array is too small, not of objects, or there are no parallel workers.
fn likely_element_starts(bytes: &[u8], first: usize) -> Option<(Vec<usize>, ElementShape<'_>)> {
    let span = bytes.len() - first;
    if span < PARALLEL_SCAN_LEN
        || !profiles::has_parallel_workers()
        || bytes.get(first) != Some(&b'{')
    {
        return None;
    }
    let shape = ElementShape::of_object(bytes, first)?;

    let mut starts = vec![first];
    let group_count =
        (span / MIN_GROUP_LEN).clamp(1, GROUPS_PER_THREAD * rayon::current_num_threads());
    for group in 1..group_count {
        let previous = starts[starts.len() - 1];
        let target = (first + span * group / group_count).max(previous + 1);
        match shape.starts_between(bytes, target, bytes.len()).next() {
            Some(start) => starts.push(start),
            None => break,
        }
    }
    (starts.len() >= 2).then_some((starts, shape))
}

/// The first two keys of an array's first object element, used to find likely element starts.
/// Matching two keys rejects most nested objects that happen to share the first key.
pub(super) struct ElementShape<'a> {
    /// The first key, from its opening quote through its colon.
    first_key: memmem::Finder<'a>,
    /// The second key in the same form, or `None` when the object has one member.
    second_key: Option<&'a [u8]>,
}

impl<'a> ElementShape<'a> {
    pub(super) fn of_object(bytes: &'a [u8], open: usize) -> Option<Self> {
        let key_start = skip_ws(bytes, open + 1);
        let (_, colon) = member_header(bytes, key_start)?;
        let after_value = skip_ws(bytes, skip_value(bytes, skip_ws(bytes, colon + 1)));
        let second_key = match bytes.get(after_value) {
            Some(b'}') => None,
            Some(b',') => {
                let second_start = skip_ws(bytes, after_value + 1);
                Some(&bytes[second_start..=member_header(bytes, second_start)?.1])
            }
            _ => return None,
        };
        Some(Self {
            first_key: memmem::Finder::new(&bytes[key_start..=colon]),
            second_key,
        })
    }

    /// Yields positions in `after..before` of comma-preceded objects with this shape. Keys
    /// cannot match inside strings, since a JSON string holds no unescaped quote.
    pub(super) fn starts_between(
        &'a self,
        bytes: &'a [u8],
        after: usize,
        before: usize,
    ) -> impl Iterator<Item = usize> + 'a {
        let preceding = |position: usize| {
            bytes[..position]
                .iter()
                .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        };
        let key_len = self.first_key.needle().len();
        self.first_key
            .find_iter(&bytes[after..before])
            .map(move |offset| after + offset)
            .filter_map(move |key| {
                let open = preceding(key).filter(|&open| bytes[open] == b'{' && open > after)?;
                (preceding(open).is_some_and(|comma| bytes[comma] == b',')
                    && self.continues_after_first_key(bytes, key + key_len))
                .then_some(open)
            })
    }

    /// Whether the value at `value_start` is followed by the second key, or by `}` if none.
    fn continues_after_first_key(&self, bytes: &[u8], value_start: usize) -> bool {
        let after_value = skip_ws(bytes, skip_value(bytes, skip_ws(bytes, value_start)));
        match (bytes.get(after_value), self.second_key) {
            (Some(b'}'), None) => true,
            (Some(b','), Some(second_key)) => {
                bytes[skip_ws(bytes, after_value + 1)..].starts_with(second_key)
            }
            _ => false,
        }
    }
}
