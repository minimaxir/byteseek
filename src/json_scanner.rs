use std::borrow::Cow;

use bytes::{container_end_within, skip_string, skip_value, skip_ws};
use memchr::{memchr, memrchr};

mod batch;
/// Shared JSON-compatible double-quoted string scanning.
pub(crate) mod bytes;
/// Shared hit collection policy for the line-oriented structured scanners.
pub(crate) mod collection;
mod parallel;
mod traversal;

pub(crate) use batch::JsonPathBatch;
pub(crate) use parallel::PARALLEL_SCAN_LEN as JSON_PARALLEL_SCAN_LEN;
pub(crate) use traversal::{
    json_fold_small_document, json_fold_values_with_paths, json_scan_values,
    json_scan_values_prefiltered, json_scan_values_with_paths,
};

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::json_value_stream;

/// How far from each end of a document the JSON Lines probes look for a line break, which
/// bounds their cost however large the document is.
const STREAM_PROBE_LEN: usize = 4096;

/// Where the values of a stream of JSON values, such as JSON Lines, lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JsonStream {
    /// The span of the first value.
    pub(crate) first: (usize, usize),
    /// Where the last value ends.
    pub(crate) end: usize,
}

/// Returns the extent of a stream of JSON values, such as JSON Lines, or `None` for a single
/// document. A stream is recognized when its first or last line, found within
/// [`STREAM_PROBE_LEN`] bytes of its end of the document, holds a whole value with more
/// beside it, which no single document does.
pub(crate) fn json_stream_extent(bytes: &[u8]) -> Option<JsonStream> {
    let start = skip_ws(bytes, 0);
    let end = bytes.iter().rposition(|byte| !byte.is_ascii_whitespace())? + 1;
    let first_line_holds_value = || {
        let probe = &bytes[start..end.min(start + STREAM_PROBE_LEN)];
        value_end_within(bytes, start, start + memchr(b'\n', probe)?)
    };
    let last_line_holds_value = || {
        let probe_start = end.saturating_sub(STREAM_PROBE_LEN).max(start);
        let line_start = probe_start + memrchr(b'\n', &bytes[probe_start..end])? + 1;
        value_end_within(bytes, skip_ws(bytes, line_start), end)
    };
    first_line_holds_value().or_else(last_line_holds_value)?;
    let (first, second) = json_stream_value(bytes, start, end)?;
    (second < end).then_some(JsonStream { first, end })
}

/// Returns the span of the value at `start` in a stream of JSON values whose last one ends at
/// `content_end`, and where the value after it starts, which is `content_end` after the last;
/// `None` for an unparseable value, which the stream ends with.
pub(crate) fn json_stream_value(
    bytes: &[u8],
    start: usize,
    content_end: usize,
) -> Option<((usize, usize), usize)> {
    let end = skip_value(bytes, start);
    (end > start).then(|| {
        (
            (start, end.min(content_end)),
            skip_ws(bytes, end).min(content_end),
        )
    })
}

/// Returns [`json_stream_value`]'s span and next value for a value shorter than `max_len`
/// bytes, reading no more than that of a longer array or object; `None` for a longer value
/// or an unparseable one.
pub(crate) fn json_short_stream_value(
    bytes: &[u8],
    start: usize,
    content_end: usize,
    max_len: usize,
) -> Option<((usize, usize), usize)> {
    let limit = start.saturating_add(max_len.saturating_sub(1));
    let end = value_end_within(bytes, start, limit.min(content_end))?;
    Some(((start, end), skip_ws(bytes, end).min(content_end)))
}

/// Returns where the first line after `target` holds its first byte that is not whitespace,
/// which is where a value of JSON Lines starts; `None` if no line after it holds one.
pub(crate) fn json_line_value_start(bytes: &[u8], target: usize) -> Option<usize> {
    let line = target + memchr(b'\n', &bytes[target..])? + 1;
    Some(skip_ws(bytes, line)).filter(|&start| start < bytes.len())
}

/// Returns where the JSON value at `start` ends if it ends by `limit`.
fn value_end_within(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    match *bytes.get(start)? {
        b'{' => container_end_within(bytes, start, (b'{', b'}'), limit),
        b'[' => container_end_within(bytes, start, (b'[', b']'), limit),
        _ => Some(skip_value(bytes, start)).filter(|&end| start < end && end <= limit),
    }
}

/// Returns `(key_start, key_end, value_start)` for the object member at `position`.
#[inline(always)]
fn object_member(bytes: &[u8], position: usize) -> Option<(usize, usize, usize)> {
    let (string_end, colon) = member_header(bytes, position)?;
    Some((position + 1, string_end - 1, skip_ws(bytes, colon + 1)))
}

/// Locates the key string's end and colon without scanning its value.
#[inline(always)]
fn member_header(bytes: &[u8], position: usize) -> Option<(usize, usize)> {
    if bytes.get(position) != Some(&b'"') {
        return None;
    }
    let string_end = skip_string(bytes, position);
    let colon = skip_ws(bytes, string_end);
    (bytes.get(colon) == Some(&b':')).then_some((string_end, colon))
}

/// Finds the next member or element after a value, stopping at any other separator.
#[inline(always)]
fn next_item(bytes: &[u8], end: usize) -> Option<usize> {
    let separator = skip_ws(bytes, end);
    (bytes.get(separator) == Some(&b',')).then(|| skip_ws(bytes, separator + 1))
}

/// A path segment for navigating structured documents.
#[derive(Debug, Clone)]
pub(crate) enum PathSegment {
    Key(String),
    Index(usize),
}

/// A parsed structured path query with optional value constraint.
#[derive(Debug)]
pub(crate) struct ParsedPathQuery {
    pub(crate) segments: Vec<PathSegment>,
    pub(crate) expected_value: Option<String>,
    pub(crate) pattern_index: usize,
}

/// Parse a structured pattern like `$.events[777].payload.user.name` or
/// `$.events[38].payload.status=failed` into path segments.
pub(crate) fn parse_path_pattern(pattern: &[u8], pattern_index: usize) -> Option<ParsedPathQuery> {
    let text = std::str::from_utf8(pattern).ok()?;
    if !text.starts_with('$') {
        return None;
    }

    let path_bytes = text.as_bytes();
    let mut segments = Vec::new();
    let mut pos = 1;
    // The path ends at an `=` outside brackets, which starts the expected value.
    while pos < path_bytes.len() && path_bytes[pos] != b'=' {
        match path_bytes[pos] {
            b'.' => {
                pos += 1;
                let start = pos;
                while pos < path_bytes.len() && !matches!(path_bytes[pos], b'.' | b'[' | b'=') {
                    pos += 1;
                }
                if pos > start {
                    segments.push(PathSegment::Key(text[start..pos].to_string()));
                }
            }
            b'[' if path_bytes.get(pos + 1) == Some(&b'"') => {
                let start = pos + 1;
                pos = start + 1;
                while pos < path_bytes.len() && path_bytes[pos] != b'"' {
                    pos += 1 + usize::from(path_bytes[pos] == b'\\');
                }
                // An escape or the key itself ran into the end: the quote never closes.
                if pos >= path_bytes.len() {
                    return None;
                }
                pos += 1;
                let quoted = &text[start..pos];
                let key = if quoted.contains('\\') {
                    serde_json::from_str(quoted).ok()?
                } else {
                    quoted[1..quoted.len() - 1].to_string()
                };
                segments.push(PathSegment::Key(key));
                if path_bytes.get(pos) == Some(&b']') {
                    pos += 1;
                }
            }
            b'[' => {
                let start = pos + 1;
                pos = start;
                while pos < path_bytes.len() && path_bytes[pos] != b']' {
                    pos += 1;
                }
                segments.push(PathSegment::Index(text[start..pos].parse().ok()?));
                pos += 1;
            }
            _ => return None,
        }
    }
    let expected_value = text.get(pos + 1..).map(str::to_string);

    Some(ParsedPathQuery {
        segments,
        expected_value,
        pattern_index,
    })
}

/// Find a key in a JSON object. `pos` points to `{`.
/// Returns the position of the value after the colon.
#[inline]
fn find_key_in_object(bytes: &[u8], pos: usize, target_key: &[u8]) -> Option<usize> {
    debug_assert!(bytes[pos] == b'{');
    let mut position = skip_ws(bytes, pos + 1);
    loop {
        let (start, end, value) = object_member(bytes, position)?;
        if json_key_matches(&bytes[start..end], target_key) {
            return Some(value);
        }
        position = next_item(bytes, skip_value(bytes, value))?;
    }
}

/// Returns whether the raw body of a JSON key string spells `key` once decoded. Escapes only
/// shorten a key, so a raw key as long as `key` must match it verbatim and without escapes.
#[inline(always)]
fn json_key_matches(raw: &[u8], key: &[u8]) -> bool {
    if raw.len() == key.len() {
        return raw == key && !raw.contains(&b'\\');
    }
    raw.len() > key.len() && raw.contains(&b'\\') && unescape_json_string(raw).as_bytes() == key
}

/// Returns the decoded text of a raw JSON key when it has escapes, or `None` when it reads as
/// written.
#[inline]
pub(crate) fn decoded_json_key(raw: &[u8]) -> Option<String> {
    raw.contains(&b'\\').then(|| unescape_json_string(raw))
}

/// Find an element by index in a JSON array. `pos` points to `[`.
/// Returns the position of the element value.
#[inline]
fn find_index_in_array(bytes: &[u8], pos: usize, target_index: usize) -> Option<usize> {
    debug_assert!(bytes[pos] == b'[');
    let mut position = skip_ws(bytes, pos + 1);
    if bytes.get(position) == Some(&b']') {
        return None;
    }
    for _ in 0..target_index {
        if position >= bytes.len() {
            return None;
        }
        position = next_item(bytes, skip_value(bytes, position))?;
    }
    (position < bytes.len()).then_some(position)
}

/// Navigate a JSON document along path segments.
///
/// # Returns
///
/// `(start, end)` byte offsets of the target value, or `None` if not found.
#[inline]
pub(crate) fn json_navigate_path(bytes: &[u8], segments: &[PathSegment]) -> Option<(usize, usize)> {
    let mut pos = skip_ws(bytes, 0);

    for segment in segments {
        pos = match (bytes.get(pos), segment) {
            (Some(b'{'), PathSegment::Key(key)) => find_key_in_object(bytes, pos, key.as_bytes())?,
            (Some(b'['), PathSegment::Index(index)) => find_index_in_array(bytes, pos, *index)?,
            _ => return None,
        };
    }

    let start = pos;
    let end = skip_value(bytes, pos);
    Some((start, end))
}

/// Extract the scalar text from a JSON value at `[start, end)`.
/// For strings, returns the unquoted content. For numbers/bools/null,
/// returns the raw text. For containers, returns `None`.
#[inline]
pub(crate) fn json_scalar_text_at<'a>(
    bytes: &'a [u8],
    start: usize,
    end: usize,
) -> Option<Cow<'a, str>> {
    if start >= bytes.len() {
        return None;
    }
    match bytes[start] {
        b'"' => {
            let inner = &bytes[start + 1..end - 1];
            if !inner.contains(&b'\\') {
                std::str::from_utf8(inner).ok().map(Cow::Borrowed)
            } else {
                Some(Cow::Owned(unescape_json_string(inner)))
            }
        }
        b'{' | b'[' => None,
        _ => std::str::from_utf8(&bytes[start..end])
            .ok()
            .map(Cow::Borrowed),
    }
}

/// Decodes the escapes of a JSON string body. Unknown escapes are kept verbatim and a `\u`
/// escape that is not a valid character is dropped.
fn unescape_json_string(inner: &[u8]) -> String {
    fn code_unit(inner: &[u8], at: usize) -> Option<u32> {
        let digits = std::str::from_utf8(inner.get(at..at + 4)?).ok()?;
        u32::from_str_radix(digits, 16).ok()
    }

    // Built as bytes so the text between escapes is copied without decoding.
    let mut result = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        if inner[i] != b'\\' || i + 1 == inner.len() {
            result.push(inner[i]);
            i += 1;
            continue;
        }
        match inner[i + 1] {
            escaped @ (b'"' | b'\\' | b'/') => result.push(escaped),
            b'n' => result.push(b'\n'),
            b'r' => result.push(b'\r'),
            b't' => result.push(b'\t'),
            b'b' => result.push(0x08),
            b'f' => result.push(0x0C),
            b'u' if i + 5 < inner.len() => {
                let mut escape_len = 6;
                let mut code_point = code_unit(inner, i + 2);
                // Characters above U+FFFF are written as a surrogate pair.
                if let Some(high @ 0xD800..=0xDBFF) = code_point
                    && inner.get(i + 6..i + 8) == Some(b"\\u")
                    && let Some(low @ 0xDC00..=0xDFFF) = code_unit(inner, i + 8)
                {
                    code_point = Some(0x1_0000 + ((high - 0xD800) << 10) + (low - 0xDC00));
                    escape_len = 12;
                }
                if let Some(character) = code_point.and_then(char::from_u32) {
                    result.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
                }
                i += escape_len;
                continue;
            }
            other => result.extend_from_slice(&[b'\\', other]),
        }
        i += 2;
    }
    // Valid UTF-8 input stays valid; anything else is replaced lossily.
    String::from_utf8(result)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// Result from scanning all values in a JSON document.
pub(crate) use collection::{HitPath, ScalarHit as JsonScalarHit};
