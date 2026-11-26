use std::cell::Cell;
use std::num::NonZeroUsize;
use std::ops::Range;

use memchr::{memchr, memchr_iter, memchr2, memrchr};

use lines::{ContentLine, next_content_line, next_line, skip_spaces};
use navigation::{after_yaml_colon, find_key_colon};
use parallel::map_yaml_chunks;
use scalar::{is_escaped_or_open_quoted_scalar, scalar_extent};

use crate::jq_path::{append_array_segment, append_key_span};
use crate::json_scanner::collection::{HitPath, collect_hits, push_hit};
use crate::occurrences::{Needle, OccurrenceCursor};

/// Shared line cursors with format-specific whitespace rules.
pub(crate) mod lines;
mod navigation;
mod parallel;
/// Shared allocation-free scalar/path byte helpers for the structured scanners.
pub(crate) mod scalar;

pub(crate) use navigation::yaml_navigate_path;

#[cfg(test)]
mod tests;

/// Result from scanning YAML scalar values.
pub(crate) use crate::json_scanner::collection::ScalarHit as YamlScalarHit;

/// Returns up to `limit` scalar values of a block-style YAML document for which `matcher`
/// returns true, or `None` when the document needs the full parser.
pub(crate) fn yaml_scan_values<F, P>(
    bytes: &[u8],
    limit: usize,
    matcher: F,
) -> Option<Vec<YamlScalarHit<P>>>
where
    F: Fn(&[u8]) -> bool + Sync,
    P: HitPath,
{
    scan_yaml_hits(bytes, limit, &matcher, None)
}

/// Folds every scalar into `init()` states, one per chunk in document order, skipping sequence
/// items without a `prefilter` occurrence. Returns `None` when the document needs the full
/// parser.
pub(crate) fn yaml_fold_values_with_paths<S, I, V>(
    bytes: &[u8],
    prefilter: Option<&Needle>,
    init: I,
    visit: V,
) -> Option<Vec<S>>
where
    S: Send,
    I: Fn() -> S + Sync,
    V: Fn(&mut S, &[u8], usize, usize) + Sync,
{
    map_yaml_chunks(bytes, |range, state| {
        let mut folded = init();
        let mut cursor = prefilter.map(|needle| OccurrenceCursor::new(needle, bytes));
        let reached = yaml_traverse_values::<false, _, _, _>(
            bytes,
            range,
            state,
            |path, _, start, end| visit(&mut folded, path, start, end),
            |item_start, content_start, indent| {
                cursor
                    .as_mut()
                    .and_then(|cursor| {
                        skipped_item_end(cursor, bytes, (item_start, content_start), indent)
                    })
                    // An alias may expand to a match, so read the item and let the alias
                    // send the document to the full parser.
                    .filter(|&item_end| memchr(b'*', &bytes[item_start..item_end]).is_none())
            },
            || false,
        );
        (folded, reached)
    })
}

/// Like [`yaml_scan_values`], but skips sequence items that do not contain `needle`.
pub(crate) fn yaml_scan_values_prefiltered<F, P>(
    bytes: &[u8],
    needle: &Needle,
    limit: usize,
    matcher: F,
) -> Option<Vec<YamlScalarHit<P>>>
where
    F: Fn(&[u8]) -> bool + Sync,
    P: HitPath,
{
    scan_yaml_hits(bytes, limit, &matcher, Some(needle))
}

fn scan_yaml_hits<F, P>(
    bytes: &[u8],
    limit: usize,
    matcher: &F,
    prefilter: Option<&Needle>,
) -> Option<Vec<YamlScalarHit<P>>>
where
    F: Fn(&[u8]) -> bool + Sync,
    P: HitPath,
{
    if limit == usize::MAX {
        collect_yaml_hits::<false, _, _>(bytes, limit, matcher, prefilter)
    } else {
        collect_yaml_hits::<true, _, _>(bytes, limit, matcher, prefilter)
    }
}

/// Returns the end of the sequence item at `indent` if it holds no needle occurrence.
fn skipped_item_end(
    cursor: &mut OccurrenceCursor<'_>,
    bytes: &[u8],
    (item_start, content_start): (usize, usize),
    indent: usize,
) -> Option<usize> {
    cursor.skippable_end(item_start, |limit| {
        find_section_end::<64>(bytes, content_start, indent, limit)
    })
}

// `LIMITED` is const so unlimited scans skip the hit counting and stop checks.
fn collect_yaml_hits<const LIMITED: bool, F, P>(
    bytes: &[u8],
    limit: usize,
    matcher: &F,
    prefilter: Option<&Needle>,
) -> Option<Vec<YamlScalarHit<P>>>
where
    F: Fn(&[u8]) -> bool + Sync,
    P: HitPath,
{
    collect_hits(
        (bytes.len(), limit),
        YamlScanState::new,
        |range, state| {
            let mut hits = Vec::new();
            let hit_count = Cell::new(0);
            let mut cursor = prefilter.map(|needle| OccurrenceCursor::new(needle, bytes));
            let reached = yaml_traverse_values::<true, _, _, _>(
                bytes,
                range,
                state,
                |path, key, start, end| {
                    let (match_start, match_end) =
                        if end - start >= 2 && matches!(bytes[start], b'"' | b'\'') {
                            (start + 1, end - 1)
                        } else {
                            (start, end)
                        };
                    if matcher(&bytes[match_start..match_end]) {
                        with_leaf_path(path, (bytes, key), |path| {
                            push_hit(&mut hits, path, start, end);
                        });
                    }
                    if LIMITED {
                        hit_count.set(hits.len());
                    }
                },
                |item_start, content_start, indent| {
                    cursor.as_mut().and_then(|cursor| {
                        skipped_item_end(cursor, bytes, (item_start, content_start), indent)
                    })
                },
                || LIMITED && hit_count.get() >= limit,
            );
            (hits, reached)
        },
        |scan| map_yaml_chunks(bytes, scan),
    )
}

/// A leaf key not yet appended to the path, as `(start, end + 1)`; `None` if there is none.
/// Storing `end + 1` as `NonZeroUsize` keeps the `Option` the size of the tuple.
type LeafKey = Option<(usize, NonZeroUsize)>;

/// Appends the leaf key to `path` for the duration of `visit`.
#[inline(always)]
fn with_leaf_path(
    path: &mut Vec<u8>,
    (bytes, key): (&[u8], LeafKey),
    visit: impl FnOnce(&mut Vec<u8>),
) {
    let parent_len = path.len();
    if let Some((start, end)) = key {
        append_yaml_key(path, bytes, start, end.get() - 1);
    }
    visit(path);
    path.truncate(parent_len);
}

/// The traversal's state at a line start: the open block frames and the current path.
#[derive(Clone, Debug, PartialEq)]
struct YamlScanState {
    path_stack: Vec<YamlPathFrame>,
    path: Vec<u8>,
}

impl YamlScanState {
    fn new() -> Self {
        let mut path = Vec::with_capacity(128);
        path.push(b'$');
        Self {
            path_stack: Vec::with_capacity(16),
            path,
        }
    }
}

/// Traverses the lines starting in `range`, returning where it stopped (`range.end` unless
/// `should_stop` fired or a skipped item ran past it), or `None` for unsupported syntax.
///
/// With `LAZY_PATH`, `callback` gets the parent path plus a deferred leaf key, which it must
/// remove again if it appends it; otherwise it gets the full path.
fn yaml_traverse_values<const LAZY_PATH: bool, C, P, S>(
    bytes: &[u8],
    mut range: Range<usize>,
    state: &mut YamlScanState,
    mut callback: C,
    mut skipped_sequence_end: P,
    mut should_stop: S,
) -> Option<usize>
where
    C: FnMut(&mut Vec<u8>, LeafKey, usize, usize),
    P: FnMut(usize, usize, usize) -> Option<usize>,
    S: FnMut() -> bool,
{
    while let Some(line) = next_content_line(bytes, &mut range) {
        let indent = line.indent;
        let content_start = line.content;

        let is_sequence_item = line.is_sequence_item(bytes);
        // A tab is no indentation: the parser rejects the line.
        if matches!(bytes[content_start], b'{' | b'[' | b'|' | b'>' | b'\t') {
            return None;
        }

        close_yaml_frames(state, indent, is_sequence_item);

        if bytes[content_start] == b'-' && bytes[content_start..].starts_with(b"---") {
            return None;
        }

        let mut value_line = line;
        if is_sequence_item {
            update_yaml_sequence_path(state, indent);
            if let Some(item_end) = skipped_sequence_end(line.start, content_start, indent) {
                range.start = item_end;
                continue;
            }
            // Extra spaces after the dash shift the item's indentation; leave that to the parser.
            if bytes.get(content_start + 2) == Some(&b' ') {
                return None;
            }
            value_line.content += 2;
            value_line.indent += 2;
            // The line-start check above saw only the dash, not the item itself.
            if starts_unsupported_value(bytes, value_line.content)
                || value_line.is_sequence_item(bytes)
            {
                return None;
            }
        }
        let line_kind = (value_line, is_sequence_item);
        if !visit_yaml_line::<LAZY_PATH, _>(bytes, line_kind, state, &mut callback) {
            return None;
        }
        if should_stop() {
            return Some(range.start);
        }
    }

    Some(range.start)
}

/// Pops the frames a line at `indent` closes. A sequence item keeps its sequence open.
fn close_yaml_frames(state: &mut YamlScanState, indent: usize, is_sequence_item: bool) {
    while let Some(frame) = state.path_stack.last() {
        let closes = if frame.is_sequence && is_sequence_item {
            indent < frame.indent
        } else {
            indent <= frame.indent
        };
        if !closes {
            break;
        }
        state.path.truncate(frame.path_len);
        state.path_stack.pop();
    }
}

fn update_yaml_sequence_path(state: &mut YamlScanState, indent: usize) {
    if let Some(frame) = state.path_stack.last_mut()
        && frame.is_sequence
        && frame.indent == indent
    {
        let index = frame.array_index;
        frame.array_index += 1;
        state.path.truncate(frame.path_len);
        append_array_segment(&mut state.path, index);
        return;
    }

    let path_len = state.path.len();
    state.path_stack.push(YamlPathFrame {
        indent,
        path_len,
        is_sequence: true,
        array_index: 1,
    });
    append_array_segment(&mut state.path, 0);
}

/// Visits one content line. Returns `false` when it needs the full parser.
fn visit_yaml_line<const LAZY_PATH: bool, C>(
    bytes: &[u8],
    (line, is_sequence_item): (ContentLine, bool),
    state: &mut YamlScanState,
    callback: &mut C,
) -> bool
where
    C: FnMut(&mut Vec<u8>, LeafKey, usize, usize),
{
    let Some((key_end, value_info)) = try_parse_mapping_entry(bytes, line.content, line.end) else {
        // A bare scalar line is fine as a sequence item; otherwise it continues a multi-line
        // scalar or is the whole document.
        if !is_sequence_item {
            return false;
        }
        let (start, end) = scalar_extent(bytes, line.content, || {
            plain_scalar_end(bytes, line.content, line.end)
        });
        if start < end {
            if is_parser_only_scalar(bytes, (start, end), line.end) {
                return false;
            }
            callback(&mut state.path, None, start, end);
        }
        return true;
    };

    if starts_unsupported_key(bytes, line.content) {
        return false;
    }
    match value_info {
        Some(scalar) if is_parser_only_scalar(bytes, scalar, line.end) => return false,
        Some((start, end)) => {
            let key = (
                line.content,
                NonZeroUsize::new(key_end + 1).expect("YAML key end + 1 must be nonzero"),
            );
            if LAZY_PATH {
                callback(&mut state.path, Some(key), start, end);
            } else {
                with_leaf_path(&mut state.path, (bytes, Some(key)), |path| {
                    callback(path, None, start, end);
                });
            }
        }
        None => {
            let parent_path_len = state.path.len();
            append_yaml_key(&mut state.path, bytes, line.content, key_end);
            state.path_stack.push(YamlPathFrame {
                indent: line.indent,
                path_len: parent_path_len,
                is_sequence: false,
                array_index: 0,
            });
        }
    }
    true
}

/// Appends the plain key at `bytes[start..end]`, without the spaces before its colon.
#[inline(always)]
fn append_yaml_key(path: &mut Vec<u8>, bytes: &[u8], start: usize, mut end: usize) {
    while end > start && matches!(bytes[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    append_key_span(path, bytes, start, end);
}

/// Whether the line at `start` begins with a single-quoted, complex, flow, anchored, tagged,
/// aliased, or merge (`<<`) key, which only the full parser reads correctly.
fn starts_unsupported_key(bytes: &[u8], start: usize) -> bool {
    matches!(
        bytes.get(start),
        Some(
            b'\''
                | b'?'
                | b'['
                | b'{'
                | b'&'
                | b'!'
                | b'*'
                | b'|'
                | b'>'
                | b'%'
                | b'@'
                | b'`'
                | b'<'
        )
    )
}

/// Whether the value at `start` is a flow collection, block scalar, anchor, tag or alias,
/// which only the full parser reads correctly.
fn starts_unsupported_value(bytes: &[u8], start: usize) -> bool {
    // A document may end right after a key or dash, with no value to read.
    matches!(
        bytes.get(skip_spaces(bytes, start)),
        Some(b'[' | b'{' | b'|' | b'>' | b'&' | b'!' | b'*')
    )
}

/// Whether only the full parser reads the scalar at `start..end` of a line that ends at
/// `line_end` correctly: an unsupported value, a quoted scalar that runs past its line, which
/// the parser folds or cuts off, or one that [`is_escaped_or_open_quoted_scalar`].
fn is_parser_only_scalar(bytes: &[u8], (start, end): (usize, usize), line_end: usize) -> bool {
    end > line_end
        || starts_unsupported_value(bytes, start)
        || is_escaped_or_open_quoted_scalar(bytes, start, end)
}

/// Returns where the plain scalar at `start` ends: before a ` #` comment, or at `line_end`.
fn plain_scalar_end(bytes: &[u8], start: usize, line_end: usize) -> usize {
    memchr_iter(b'#', &bytes[start..line_end])
        .map(|offset| start + offset)
        .find(|&hash| hash > start && matches!(bytes[hash - 1], b' ' | b'\t'))
        .unwrap_or(line_end)
}

#[derive(Clone, Debug, PartialEq)]
struct YamlPathFrame {
    indent: usize,
    path_len: usize,
    is_sequence: bool,
    array_index: usize,
}

type MappingEntry = (usize, Option<(usize, usize)>);

/// Parses `key: value` or `key:` into the key's end and the inline value span, which is
/// `None` when the value is a nested block.
fn try_parse_mapping_entry(bytes: &[u8], start: usize, line_end: usize) -> Option<MappingEntry> {
    // Quoted keys are left to the full parser.
    let first = start + memchr2(b':', b'"', &bytes[start..line_end])?;
    if bytes[first] == b'"' {
        return None;
    }
    let colon = if matches!(bytes.get(first + 1), None | Some(b' ' | b'\n')) {
        first
    } else {
        find_key_colon(bytes, first + 1, line_end)?
    };
    Some((
        colon,
        inline_scalar_extent(bytes, after_yaml_colon(bytes, colon), line_end),
    ))
}

/// Returns the span of the inline scalar after a colon; `None` if empty or comment-only.
fn inline_scalar_extent(bytes: &[u8], start: usize, line_end: usize) -> Option<(usize, usize)> {
    let start = skip_spaces(bytes, start);
    if start >= line_end || bytes[start] == b'#' {
        return None;
    }
    let (start, end) = scalar_extent(bytes, start, || plain_scalar_end(bytes, start, line_end));
    (start < end).then_some((start, end))
}

/// Returns the start of the first content line after `start`'s line indented at most
/// `max_indent`, or `None` if none starts at or before `limit`. Blocks of `BLOCK_LEN` bytes are
/// tested at once: short blocks find nearby ends sooner, long ones scan far ends faster.
fn find_section_end<const BLOCK_LEN: usize>(
    bytes: &[u8],
    start: usize,
    max_indent: usize,
    limit: usize,
) -> Option<usize> {
    let mut block_start = next_line(bytes, start);
    while block_start <= limit && block_start < bytes.len() {
        let block_end = (block_start + BLOCK_LEN).min(bytes.len());
        if may_start_shallow_line(bytes, block_start..block_end, max_indent) {
            // Earlier lines were all deeper, so walk from the last line start before here.
            let walk_start = memrchr(b'\n', &bytes[..block_start]).map_or(0, |newline| newline + 1);
            if let Some(end) = shallow_line_start(bytes, walk_start..block_end, max_indent) {
                return (end <= limit).then_some(end);
            }
        }
        block_start = block_end;
    }
    None
}

/// Whether a line starting in `block` might be indented at most `max_indent`; false only when
/// every such line starts with `max_indent + 1` spaces. The non-short-circuit `&` and `|` keep
/// the loops branch-free so they vectorize.
fn may_start_shallow_line(bytes: &[u8], block: Range<usize>, max_indent: usize) -> bool {
    let starts_line = block.start == 0 || bytes[block.start - 1] == b'\n';
    if starts_line
        && !bytes
            .get(block.start..=block.start + max_indent)
            .is_some_and(|prefix| prefix.iter().all(|&byte| byte == b' '))
    {
        return true;
    }
    let newlines = &bytes[block.clone()];
    (1..=max_indent + 1).any(|column| {
        // For a newline at `i`, `bytes[i + column]` is column `column - 1` of the next line.
        let Some(shifted) = bytes.get(block.start + column..block.end + column) else {
            return true;
        };
        newlines
            .iter()
            .zip(shifted)
            .fold(false, |found, (&byte, &at_column)| {
                found | ((byte == b'\n') & (at_column != b' '))
            })
    })
}

/// Returns the first content line in `range` (which begins at a line start) indented at most
/// `max_indent`, skipping blank and comment lines.
fn shallow_line_start(bytes: &[u8], range: Range<usize>, max_indent: usize) -> Option<usize> {
    let mut position = range.start;
    while position < range.end {
        let content = skip_spaces(bytes, position);
        if content < bytes.len()
            && !matches!(bytes[content], b'\n' | b'#')
            && content - position <= max_indent
        {
            return Some(position);
        }
        position = next_line(bytes, content);
    }
    None
}
