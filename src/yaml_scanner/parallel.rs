use std::ops::Range;

use memchr::{memmem, memrchr};

use super::lines::{count_indent, sequence_item_prefix};
use super::{YamlScanState, close_yaml_frames, update_yaml_sequence_path, yaml_traverse_values};
use crate::chunks::map_chunks;
use crate::profiles;

/// Smallest document traversed in parallel chunks.
pub(super) const PARALLEL_SCAN_LEN: usize = 64 * 1024;
/// Smallest chunk worth a parallel task.
const MIN_CHUNK_LEN: usize = 16 * 1024;
/// Chunks per rayon thread, so work stealing can balance uneven chunks.
const CHUNKS_PER_THREAD: usize = 4;

/// Runs `scan` (a [`yaml_traverse_values`] wrapper) over line-aligned chunks in parallel and
/// returns the outputs in order, or `None` for unsupported syntax.
///
/// Chunks after the first start at a sequence item line with a speculated state (see
/// [`YamlChunkPlan`]). A chunk is kept if the previous chunk ended in the state it assumed;
/// otherwise it is rescanned from the true state.
pub(super) fn map_yaml_chunks<T, F>(bytes: &[u8], scan: F) -> Option<Vec<T>>
where
    T: Send,
    F: Fn(Range<usize>, &mut YamlScanState) -> (T, Option<usize>) + Sync,
{
    let Some(plan) = YamlChunkPlan::new(bytes) else {
        let (output, reached) = scan(0..bytes.len(), &mut YamlScanState::new());
        return reached.map(|_| vec![output]);
    };
    let chunk_range = |chunk: usize| {
        plan.starts[chunk]..plan.starts.get(chunk + 1).copied().unwrap_or(bytes.len())
    };
    let link = |chunk: usize, position: usize, state: YamlScanState| {
        let end = chunk_range(chunk).end;
        if position != end {
            Link::Overran(position, state)
        } else if end == bytes.len() || plan.leads_into(chunk + 1, &state) {
            Link::Held
        } else {
            Link::Broke(state)
        }
    };

    let chunks = map_chunks(plan.starts.len(), |chunk| {
        let mut state = plan.start_states[chunk].clone();
        let (output, reached) = scan(chunk_range(chunk), &mut state);
        (output, reached.map(|position| link(chunk, position, state)))
    });

    let mut outputs = Vec::with_capacity(chunks.len() + 1);
    // Set when the current chunk's assumed start state was wrong.
    let mut true_state = None;
    for (chunk, (mut output, mut linked)) in chunks.into_iter().enumerate() {
        if let Some(mut state) = true_state.take() {
            let reached;
            (output, reached) = scan(chunk_range(chunk), &mut state);
            linked = reached.map(|position| link(chunk, position, state));
        }
        // Unsupported syntax from the true state would stop a sequential traversal too.
        match linked? {
            Link::Held => outputs.push(output),
            Link::Broke(state) => {
                outputs.push(output);
                true_state = Some(state);
            }
            Link::Overran(position, mut state) => {
                outputs.push(output);
                let (rest, reached) = scan(position..bytes.len(), &mut state);
                reached?;
                outputs.push(rest);
                return Some(outputs);
            }
        }
    }
    Some(outputs)
}

/// How a chunk's traversal ended relative to the next chunk's assumed state.
enum Link {
    /// Ended at the next chunk's start in the assumed state.
    Held,
    /// Ended at the next chunk's start in a different state.
    Broke(YamlScanState),
    /// A skipped section ran past the next chunk's start, to this line and state.
    Overran(usize, YamlScanState),
}

/// Chunk starts for a parallel traversal and the speculated state at each.
pub(super) struct YamlChunkPlan {
    /// `0`, then starts of sequence item lines at `item_indent`.
    pub(super) starts: Vec<usize>,
    /// Each chunk's assumed state after its first line closes frames: fresh for the first
    /// chunk, otherwise with the item's sequence frame (and index) on top.
    start_states: Vec<YamlScanState>,
    item_indent: usize,
}

impl YamlChunkPlan {
    /// Plans chunks, or returns `None` when the document is small or has no sequence.
    ///
    /// A chunk's first item is assumed to belong to the latest sequence opened right below a
    /// root-level line (see [`root_line_before`]). Since a root-level line closes every frame,
    /// a short traversal from it gives the sequence's state, and counting item lines its index.
    pub(super) fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < PARALLEL_SCAN_LEN || !profiles::has_parallel_workers() {
            return None;
        }
        let first_item = first_sequence_item(bytes)?;
        let item_indent = count_indent(bytes, first_item);
        let item_line = sequence_item_prefix(item_indent);
        let finder = memmem::Finder::new(&item_line);

        let span = bytes.len() - first_item;
        let chunk_count =
            (span / MIN_CHUNK_LEN).clamp(1, CHUNKS_PER_THREAD * rayon::current_num_threads());
        let mut starts = vec![0];
        for chunk in 1..chunk_count {
            let target = (first_item + span * chunk / chunk_count).max(starts[starts.len() - 1]);
            match finder.find(&bytes[target..]) {
                Some(offset) if target + offset + 1 < bytes.len() => {
                    starts.push(target + offset + 1);
                }
                _ => break,
            }
        }
        if starts.len() < 2 {
            return None;
        }

        let segments = map_chunks(starts.len() - 1, |chunk| {
            ItemLines::in_range(bytes, &finder, starts[chunk]..starts[chunk + 1])
        });
        let mut start_states = Vec::with_capacity(starts.len());
        start_states.push(YamlScanState::new());
        let mut opening = SequenceOpening {
            root_line: 0,
            item_line: first_item,
        };
        let mut opening_state = sequence_item_state(bytes, opening, item_indent);
        let mut items_before = 0;
        for (chunk, segment) in segments.iter().enumerate() {
            match segment.last_opening {
                Some((latest, items_from_it)) => {
                    if latest != opening {
                        opening = latest;
                        opening_state = sequence_item_state(bytes, opening, item_indent);
                    }
                    items_before = items_from_it;
                }
                None => items_before += segment.count,
            }
            let Some((state, first_index)) = &opening_state else {
                // No state to assume, so the previous chunk runs to the end.
                starts.truncate(chunk + 1);
                break;
            };
            let mut state = state.clone();
            state
                .path_stack
                .last_mut()
                .expect("the item state has its sequence's frame on top")
                .array_index = first_index + items_before;
            start_states.push(state);
        }

        (starts.len() >= 2).then_some(Self {
            starts,
            start_states,
            item_indent,
        })
    }

    /// Whether `state`, reached at the start of `chunk`, matches the state it assumed. Path
    /// bytes after the sequence are ignored, since visiting the item truncates them.
    pub(super) fn leads_into(&self, chunk: usize, state: &YamlScanState) -> bool {
        let mut state = state.clone();
        close_yaml_frames(&mut state, self.item_indent, true);
        if let Some(frame) = state.path_stack.last()
            && frame.is_sequence
            && frame.indent == self.item_indent
        {
            state.path.truncate(frame.path_len);
        }
        state == self.start_states[chunk]
    }
}

/// An item line and the root-level line (or document start) a fresh traversal can start from.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SequenceOpening {
    root_line: usize,
    item_line: usize,
}

/// Returns the state at `opening`'s item line, with its sequence frame on top, and the item's
/// index. Returns `None` on unsupported syntax.
fn sequence_item_state(
    bytes: &[u8],
    opening: SequenceOpening,
    item_indent: usize,
) -> Option<(YamlScanState, usize)> {
    let mut state = YamlScanState::new();
    let reached = yaml_traverse_values::<true, _, _, _>(
        bytes,
        opening.root_line..opening.item_line,
        &mut state,
        |_, _, _, _| {},
        |_, _, _| None,
        || false,
    )?;
    if reached != opening.item_line {
        return None;
    }
    close_yaml_frames(&mut state, item_indent, true);
    update_yaml_sequence_path(&mut state, item_indent);
    let sequence = state
        .path_stack
        .last()
        .expect("a sequence item leaves its sequence's frame on top");
    let index = sequence.array_index - 1;
    state.path.truncate(sequence.path_len);
    Some((state, index))
}

/// Item lines starting in one chunk.
struct ItemLines {
    count: usize,
    /// The last item line right below a root-level line, and the item count from it onward.
    last_opening: Option<(SequenceOpening, usize)>,
}

impl ItemLines {
    fn in_range(bytes: &[u8], finder: &memmem::Finder<'_>, range: Range<usize>) -> Self {
        let needle_len = finder.needle().len();
        let from = range.start.saturating_sub(1);
        let to = (range.end + needle_len - 2).min(bytes.len());
        let mut count = usize::from(range.start == 0 && bytes.starts_with(&finder.needle()[1..]));
        let mut last_opening = None;
        for newline in finder.find_iter(&bytes[from..to]) {
            let item_line = from + newline + 1;
            if let Some(root_line) = root_line_before(bytes, item_line) {
                let opening = SequenceOpening {
                    root_line,
                    item_line,
                };
                last_opening = Some((opening, count));
            }
            count += 1;
        }
        Self {
            count,
            last_opening: last_opening.map(|(opening, before)| (opening, count - before)),
        }
    }
}

/// Returns the start of the previous content line if it is root-level: unindented and not a
/// sequence item.
pub(super) fn root_line_before(bytes: &[u8], line_start: usize) -> Option<usize> {
    let mut end = line_start;
    while end > 0 {
        let start = memrchr(b'\n', &bytes[..end - 1]).map_or(0, |newline| newline + 1);
        let content_start = start + count_indent(bytes, start);
        match bytes.get(content_start) {
            Some(b'\n' | b'#') => end = start,
            _ if content_start > start || bytes[start..].starts_with(b"- ") => return None,
            _ => return Some(start),
        }
    }
    None
}

/// Returns the start of the first line whose content starts with `- `.
fn first_sequence_item(bytes: &[u8]) -> Option<usize> {
    memmem::find_iter(bytes, b"- ").find_map(|dash| {
        let line_start = memrchr(b'\n', &bytes[..dash]).map_or(0, |newline| newline + 1);
        bytes[line_start..dash]
            .iter()
            .all(|&byte| byte == b' ')
            .then_some(line_start)
    })
}
