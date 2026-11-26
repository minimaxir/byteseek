use std::ops::Range;

use memchr::memchr;

use crate::line_end_from;

/// Byte positions of one line that is neither blank nor a comment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ContentLine {
    pub(crate) start: usize,
    pub(crate) indent: usize,
    pub(crate) content: usize,
    pub(crate) end: usize,
}

impl ContentLine {
    pub(super) fn is_sequence_item(self, bytes: &[u8]) -> bool {
        bytes[self.content..].starts_with(b"- ")
    }
}

/// Returns the next content line and advances `range.start` past it. Callers may also move
/// `range.start` themselves to skip whole sections.
#[inline]
pub(super) fn next_content_line(bytes: &[u8], range: &mut Range<usize>) -> Option<ContentLine> {
    next_line_content::<false>(bytes, range)
}

/// [`next_content_line`], also treating tabs as indentation when `TABS` is set (for TOML).
/// The returned `end` excludes the newline.
#[inline(always)]
pub(crate) fn next_line_content<const TABS: bool>(
    bytes: &[u8],
    range: &mut Range<usize>,
) -> Option<ContentLine> {
    while range.start < range.end {
        let start = range.start;
        let content = skip_inline_whitespace::<TABS>(bytes, start);
        let indent = content - start;
        if matches!(bytes.get(content), None | Some(b'\n' | b'#')) {
            range.start = next_line(bytes, content);
            continue;
        }
        let end = line_end_from(bytes, content);
        range.start = (end + 1).min(bytes.len());
        return Some(ContentLine {
            start,
            indent,
            content,
            end,
        });
    }
    None
}

#[inline]
pub(super) fn count_indent(bytes: &[u8], pos: usize) -> usize {
    skip_spaces(bytes, pos) - pos
}

/// Returns the line start following `pos`, or the input length at EOF.
#[inline]
pub(crate) fn next_line(bytes: &[u8], pos: usize) -> usize {
    memchr(b'\n', &bytes[pos..]).map_or(bytes.len(), |offset| pos + offset + 1)
}

#[inline]
pub(super) fn skip_spaces(bytes: &[u8], pos: usize) -> usize {
    skip_inline_whitespace::<false>(bytes, pos)
}

/// Skips spaces at and after `pos`, and tabs too when `TABS` is set.
#[inline]
pub(crate) fn skip_inline_whitespace<const TABS: bool>(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && (bytes[pos] == b' ' || (TABS && bytes[pos] == b'\t')) {
        pos += 1;
    }
    pos
}

/// Builds `\n` + `indent` spaces + `- `, the start of a sequence item at `indent`.
pub(super) fn sequence_item_prefix(indent: usize) -> Vec<u8> {
    let mut prefix = vec![b' '; indent + 3];
    prefix[0] = b'\n';
    prefix[indent + 1..].copy_from_slice(b"- ");
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_lines_skip_comments_and_blanks_and_keep_byte_positions() {
        let bytes = b"# note\n\n  - key: value\n   \nnext: 2";
        let mut range = 0..bytes.len();
        let item = next_content_line(bytes, &mut range).expect("sequence line");
        assert_eq!(
            item,
            ContentLine {
                start: 8,
                indent: 2,
                content: 10,
                end: 22
            }
        );
        assert!(item.is_sequence_item(bytes));
        let mapping = next_content_line(bytes, &mut range).expect("mapping line");
        assert_eq!(&bytes[mapping.content..mapping.end], b"next: 2");
        assert!(!mapping.is_sequence_item(bytes));
        assert!(next_content_line(bytes, &mut range).is_none());
        assert_eq!(range.start, bytes.len());
    }

    #[test]
    fn content_lines_respect_boundaries_and_section_jumps() {
        let bytes = b"a: 1\n\n- x\nlast: 2";
        let mut range = 0..6;
        assert!(next_content_line(bytes, &mut range).is_some());
        assert!(next_content_line(bytes, &mut range).is_none());
        assert_eq!(range.start, 6);
        range.start = 10;
        assert!(next_content_line(bytes, &mut range).is_none());
        assert_eq!(next_line(bytes, 0), 5);
        assert_eq!(count_indent(b"   x", 0), 3);
        assert_eq!(skip_spaces(b" x", 1), 1);
        assert_eq!(skip_spaces(b" \tx", 0), 1);
        assert_eq!(skip_inline_whitespace::<true>(b" \tx", 0), 2);
        let tabs = next_line_content::<true>(b"\t x", &mut (0..3)).expect("TOML content");
        assert_eq!(tabs.content, 2);
        assert_eq!(sequence_item_prefix(0), b"\n- ");
        assert_eq!(sequence_item_prefix(2), b"\n  - ");
    }
}
