use memchr::{memchr_iter, memmem, memrchr};

use super::lines::{count_indent, next_content_line, next_line, sequence_item_prefix, skip_spaces};
use super::{find_section_end, inline_scalar_extent, is_parser_only_scalar};
use crate::json_scanner::PathSegment;
use crate::line_end_from;

/// Finds the `(start, end)` span of the scalar at `segments` by tracking indentation, without
/// a full parser. Returns `None` if the path is not found or the syntax is unsupported (flow
/// style, multi-line scalars, multiple documents, merge keys).
pub(crate) fn yaml_navigate_path(bytes: &[u8], segments: &[PathSegment]) -> Option<(usize, usize)> {
    if segments.is_empty() {
        return None;
    }

    // `scan_from` is always a line start; `child_indent` is where the next segment must be,
    // starting with the root's indentation so deeper keys of the same name don't match.
    let mut scan_from = 0_usize;
    let mut child_indent = next_content_line(bytes, &mut (0..bytes.len()))?.indent;
    // The indentation of the key holding the current node, if it isn't the root.
    let mut parent_indent = None;

    for (seg_idx, segment) in segments.iter().enumerate() {
        let next_segment = segments.get(seg_idx + 1);

        match segment {
            // A `<<` key may be a merge, which only the full parser applies.
            PathSegment::Key(target_key) if target_key == "<<" => return None,
            PathSegment::Key(target_key) => {
                let after_colon =
                    find_yaml_key(bytes, scan_from, child_indent, target_key.as_bytes())?;
                let Some(next_segment) = next_segment else {
                    return extract_yaml_value_span(bytes, after_colon);
                };
                // A scalar or flow collection on the key's line has no block children.
                let value = skip_spaces(bytes, after_colon);
                if !matches!(bytes.get(value), None | Some(b'\n' | b'\r' | b'#')) {
                    return None;
                }

                let next = next_content_start(bytes, after_colon);
                let indent = count_indent(bytes, next);
                // Children sit deeper than their key, except a sequence written at its key's
                // indentation.
                let indentless_sequence = matches!(next_segment, PathSegment::Index(_))
                    && bytes[next + indent..].starts_with(b"- ");
                if indent < child_indent || (indent == child_indent && !indentless_sequence) {
                    return None;
                }
                parent_indent = Some(child_indent);
                child_indent = indent;
                scan_from = next;
            }
            PathSegment::Index(target_idx) => {
                let (item_pos, dash_indent) = find_yaml_sequence_item(
                    bytes,
                    scan_from,
                    (child_indent, parent_indent),
                    *target_idx,
                )?;

                if next_segment.is_none() {
                    // An item starting with a key is a mapping, which the parser spans.
                    let line_end = line_end_from(bytes, item_pos);
                    if find_key_colon(bytes, item_pos, line_end).is_some() {
                        return None;
                    }
                    return extract_yaml_value_span(bytes, item_pos);
                }

                // The item's keys sit at `dash_indent + 2`, starting on the dash line itself.
                child_indent = dash_indent + 2;
                scan_from = item_pos - 2 - dash_indent;
            }
        }
    }

    None
}

/// Returns the start of the next content line after `pos`'s line.
fn next_content_start(bytes: &[u8], pos: usize) -> usize {
    next_content_line(bytes, &mut (next_line(bytes, pos)..bytes.len()))
        .map_or(bytes.len(), |line| line.start)
}

/// Finds `target_key` at `expected_indent` in the mapping starting at line `start` and returns
/// the position after its `: `. A key after `- ` on the first line counts as indented two
/// columns past the dash, as the first key of a sequence item.
fn find_yaml_key(
    bytes: &[u8],
    start: usize,
    expected_indent: usize,
    target_key: &[u8],
) -> Option<usize> {
    let mut range = start..bytes.len();
    while let Some(line) = next_content_line(bytes, &mut range) {
        let sequence = line.is_sequence_item(bytes);
        let key_start = if line.start == start && sequence && line.indent + 2 == expected_indent {
            line.content + 2
        } else if line.indent < expected_indent {
            return None;
        } else if line.indent > expected_indent || sequence {
            continue;
        } else {
            line.content
        };
        if let Some(colon) = find_key_colon(bytes, key_start, line.end)
            && &bytes[key_start..colon] == target_key
        {
            return Some(after_yaml_colon(bytes, colon));
        }
    }
    None
}

/// Returns the position of the colon separating a key from its value.
pub(super) fn find_key_colon(bytes: &[u8], start: usize, line_end: usize) -> Option<usize> {
    memchr_iter(b':', &bytes[start..line_end])
        .map(|offset| start + offset)
        .find(|&colon| matches!(bytes.get(colon + 1), None | Some(b' ' | b'\n')))
}

/// Finds the `target_idx`th item of the sequence whose first content line starts at or after
/// line `start`, with items at `indent` under a key at `parent_indent` (`None` at the root),
/// and returns `(content_after_dash, indent)`.
///
/// Items are found with `memmem`, then one pass checks that no line between the first and the
/// target is indented at most `parent_indent`, which would end the sequence first.
fn find_yaml_sequence_item(
    bytes: &[u8],
    start: usize,
    (indent, parent_indent): (usize, Option<usize>),
    target_idx: usize,
) -> Option<(usize, usize)> {
    let is_item = |line_start: usize| {
        count_indent(bytes, line_start) == indent && bytes[line_start + indent..].starts_with(b"- ")
    };
    // A nested sequence on its parent's dash line (`- - x`) is left to the parser.
    let first = next_content_line(bytes, &mut (start..bytes.len()))?.start;
    if !is_item(first) {
        return None;
    }
    // The first item has no newline before it in the searched slice.
    let item = match target_idx.checked_sub(1) {
        None => first,
        Some(later) => memmem::find_iter(&bytes[first..], &sequence_item_prefix(indent))
            .nth(later)
            .map(|offset| first + offset + 1)?,
    };
    if let Some(parent_indent) = parent_indent {
        // Sequences at their key's indentation are ended by any shallow line but an item.
        let mut line = first;
        while let Some(shallow) = find_section_end::<1024>(bytes, line, parent_indent, item) {
            if shallow == item || !is_item(shallow) {
                return (shallow == item).then_some((item + indent + 2, indent));
            }
            line = shallow;
        }
    }
    Some((item + indent + 2, indent))
}

/// Returns the inline scalar span after a key's `: `.
fn extract_yaml_value_span(bytes: &[u8], after_colon: usize) -> Option<(usize, usize)> {
    if after_colon >= bytes.len() {
        return None;
    }
    let line_end = line_end_from(bytes, after_colon);
    inline_scalar_extent(bytes, skip_spaces(bytes, after_colon), line_end)
        .filter(|&scalar| !is_parser_only_scalar(bytes, scalar, line_end))
        .filter(|&(start, _)| !plain_scalar_goes_on(bytes, start, line_end))
}

/// Whether the plain scalar at `value` continues onto the next line, which happens when that
/// line is indented deeper than the scalar's key or sequence item.
fn plain_scalar_goes_on(bytes: &[u8], value: usize, line_end: usize) -> bool {
    if matches!(bytes.get(value), Some(b'"' | b'\'')) {
        return false;
    }
    let line_start = memrchr(b'\n', &bytes[..value]).map_or(0, |newline| newline + 1);
    // The owner is the key after the line's last `- `, or that dash if the item is the scalar.
    let mut owner = skip_spaces(bytes, line_start);
    while bytes[owner..].starts_with(b"- ") {
        let after_dash = skip_spaces(bytes, owner + 2);
        if after_dash >= value {
            break;
        }
        owner = after_dash;
    }
    let mut rest = (line_end + 1).min(bytes.len())..bytes.len();
    next_content_line(bytes, &mut rest).is_some_and(|line| line.indent > owner - line_start)
}

pub(super) fn after_yaml_colon(bytes: &[u8], colon: usize) -> usize {
    colon + 1 + usize::from(bytes.get(colon + 1) == Some(&b' '))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn navigate<'a>(document: &'a str, segments: &[PathSegment]) -> Option<&'a str> {
        yaml_navigate_path(document.as_bytes(), segments).map(|(start, end)| &document[start..end])
    }

    fn key(name: &str) -> PathSegment {
        PathSegment::Key(name.to_owned())
    }

    #[test]
    fn quoted_scalars_only_the_parser_reads_are_not_navigated_to() {
        let escaped = "name: \"s\\x76c\"\n";
        let over_two_lines = "name: 'svc\n  x'\n";
        let not_closed = "name: \"svc";
        let as_written = "name: \"svc\"\n";

        let found = [escaped, over_two_lines, not_closed, as_written]
            .map(|document| navigate(document, &[key("name")]));

        assert_eq!(found[..3], [None; 3]);
        assert!(found[3].is_some());
    }

    #[test]
    fn keys_are_found_only_at_their_own_depth() {
        let nested = "events:\n  - n: 5\n";

        assert_eq!(navigate(nested, &[key("n")]), None);
        assert_eq!(
            navigate(nested, &[key("events"), PathSegment::Index(0), key("n")]),
            Some("5")
        );
        assert_eq!(navigate("a: 1\nn: 2\n", &[key("a"), key("n")]), None);
        assert_eq!(navigate("a:\nn: 2\n", &[key("a"), key("n")]), None);
    }

    #[test]
    fn sequences_end_at_the_next_line_no_deeper_than_their_key() {
        let indented = "a:\n  - 1\nb:\n  - 2\n";
        let indentless = "a:\n- 1\n- 2\nb:\n- 3\n";
        let item = |document, index| navigate(document, &[key("a"), PathSegment::Index(index)]);

        assert_eq!(item(indented, 0), Some("1"));
        assert_eq!(item(indented, 1), None);
        assert_eq!(item(indentless, 1), Some("2"));
        assert_eq!(item(indentless, 2), None);
    }

    #[test]
    fn items_that_are_mappings_or_nested_sequences_are_left_to_the_parser() {
        let document = "- x\n- n: 5\n  m: 6\n- - y\n";

        assert_eq!(navigate(document, &[PathSegment::Index(0)]), Some("x"));
        assert_eq!(navigate(document, &[PathSegment::Index(1)]), None);
        assert_eq!(
            navigate(document, &[PathSegment::Index(1), key("m")]),
            Some("6")
        );
        let nested = [PathSegment::Index(2), PathSegment::Index(0)];
        assert_eq!(navigate(document, &nested), None);
    }
}
