use memchr::memchr;

use crate::json_scanner::bytes::skip_string;

/// Returns the end of the quoted scalar at `start`, or `bytes.len()` if unterminated. Only
/// double quotes honor backslash escapes.
#[inline]
pub(crate) fn quoted_scalar_end(bytes: &[u8], start: usize) -> usize {
    if bytes[start] == b'"' {
        return skip_string(bytes, start);
    }
    memchr(bytes[start], &bytes[start + 1..]).map_or(bytes.len(), |offset| start + offset + 2)
}

/// Whether the YAML scalar at `start..end` is a quoted one whose value differs from the text
/// between its quotes, or that has no closing quote: a double-quoted one with an escape, or
/// a single-quoted one with a doubled quote, where the span [`quoted_scalar_end`] returns
/// ends early.
pub(crate) fn is_escaped_or_open_quoted_scalar(bytes: &[u8], start: usize, end: usize) -> bool {
    let quote = bytes[start];
    if !matches!(quote, b'"' | b'\'') {
        return false;
    }
    let closed = end - start >= 2 && bytes[end - 1] == quote;
    let escaped = if quote == b'"' {
        memchr(b'\\', &bytes[start..end]).is_some()
    } else {
        bytes.get(end) == Some(&quote)
    };
    !closed || escaped
}

/// Returns the span of the scalar at `start`. For plain scalars, `plain_end` gives the
/// format-specific end and trailing whitespace is trimmed.
#[inline]
pub(crate) fn scalar_extent(
    bytes: &[u8],
    start: usize,
    plain_end: impl FnOnce() -> usize,
) -> (usize, usize) {
    if start >= bytes.len() {
        return (start, start);
    }
    if matches!(bytes[start], b'"' | b'\'') {
        return (start, quoted_scalar_end(bytes, start));
    }
    let mut end = plain_end();
    while end > start && matches!(bytes[end - 1], b' ' | b'\t' | b'\r') {
        end -= 1;
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::{is_escaped_or_open_quoted_scalar, quoted_scalar_end, scalar_extent};

    #[test]
    fn quoted_scalars_preserve_escape_and_unterminated_spans() {
        for (bytes, end) in [
            (b"\"a\\\"b\" tail".as_slice(), 6),
            (b"'a\\' tail", 4),
            (b"\"a\\", 3),
            (b"'a", 2),
            (b"\"\"", 2),
        ] {
            assert_eq!(quoted_scalar_end(bytes, 0), end);
            assert_eq!(
                scalar_extent(bytes, 0, || panic!("quoted fast path")),
                (0, end)
            );
        }
        assert_eq!(scalar_extent(b" 12 \t\r", 0, || 6), (0, 3));
        assert_eq!(scalar_extent(b"", 0, || panic!("empty input")), (0, 0));
    }

    #[test]
    fn quoted_scalars_with_escapes_or_no_closing_quote_are_told_from_plain_text() {
        let as_written: [&[u8]; 5] = [b"\"a b\" tail", b"'a\\b' tail", b"''", b"plain", b"a\\b"];
        let parser_only: [&[u8]; 6] = [
            b"\"a\\x62\"",
            b"\"a\\\"",
            b"'it''s'",
            b"\"open",
            b"'open",
            b"'",
        ];
        let differs = |bytes: &[u8]| {
            let (start, end) = scalar_extent(bytes, 0, || bytes.len());
            is_escaped_or_open_quoted_scalar(bytes, start, end)
        };

        let read_as_written = as_written.map(differs);
        let left_to_the_parser = parser_only.map(differs);

        assert_eq!(read_as_written, [false; 5]);
        assert_eq!(left_to_the_parser, [true; 6]);
    }
}
