//! Writes the segments of jq-style paths such as `$.jobs["my-job"].steps[0]`.
//!
//! Every structured search writes paths through these functions, so a node prints the same
//! path whichever scanner or parser found it, and the printed path addresses it as a pattern.

use crate::append_span;

/// Appends `[index]` to `path`.
#[inline]
pub(crate) fn append_array_segment(path: &mut Vec<u8>, index: usize) {
    path.push(b'[');
    let mut buffer = itoa::Buffer::new();
    path.extend_from_slice(buffer.format(index).as_bytes());
    path.push(b']');
}

/// Appends an object key, given as UTF-8 bytes: `.key` when it is an identifier, else
/// `["key"]` with the key written as a JSON string, so keys such as `my-job`, `123`, or `a.b`
/// stay one segment.
#[inline]
pub(crate) fn append_key_segment(path: &mut Vec<u8>, key: &[u8]) {
    if is_identifier_key(key) {
        path.push(b'.');
        path.extend_from_slice(key);
    } else {
        append_quoted_key(path, key);
    }
}

/// Appends a key that isn't an identifier as `["key"]`, out of the scanners' hot loops.
#[cold]
#[inline(never)]
fn append_quoted_key(path: &mut Vec<u8>, key: &[u8]) {
    path.extend_from_slice(b"[\"");
    if key.iter().any(|&byte| needs_escape(byte)) {
        append_escaped(path, key);
    } else {
        path.extend_from_slice(key);
    }
    path.extend_from_slice(b"\"]");
}

/// Appends the key at `bytes[start..end]`, as [`append_key_segment`] would, for scanners
/// whose keys are spans of the document.
#[inline(always)]
pub(crate) fn append_key_span(path: &mut Vec<u8>, bytes: &[u8], start: usize, end: usize) {
    if is_identifier_span(bytes, start, end) {
        path.push(b'.');
        append_span(path, bytes, start, end);
    } else {
        append_quoted_key(path, &bytes[start..end]);
    }
}

/// Appends the dotted key at `bytes[start..end]`, such as `package.rust-version`, one segment
/// per part.
#[inline(always)]
pub(crate) fn append_dotted_key_span(path: &mut Vec<u8>, bytes: &[u8], start: usize, end: usize) {
    // Identifier parts joined by dots, such as `service.name`, are already written as a path.
    if is_dotted_identifier_span(bytes, start, end) {
        path.push(b'.');
        append_span(path, bytes, start, end);
    } else {
        append_dotted_key_segments(path, &bytes[start..end]);
    }
}

/// Appends the key that `bytes[start..end]` spells between the quotes of a JSON string, as
/// [`append_key_segment`] would. Only keys with escapes are decoded.
#[inline(always)]
pub(crate) fn append_json_key_segment(path: &mut Vec<u8>, bytes: &[u8], start: usize, end: usize) {
    if is_identifier_span(bytes, start, end) {
        path.push(b'.');
        append_span(path, bytes, start, end);
    } else {
        append_quoted_json_key(path, &bytes[start - 1..=end]);
    }
}

/// Appends a JSON key that isn't an identifier, given with its quotes. Kept out of line so
/// the scanners' hot loops only inline the identifier case.
#[cold]
#[inline(never)]
fn append_quoted_json_key(path: &mut Vec<u8>, quoted: &[u8]) {
    // Without escapes, the raw text is already the key as a JSON string writes it.
    if !quoted.contains(&b'\\') {
        path.push(b'[');
        path.extend_from_slice(quoted);
        path.push(b']');
        return;
    }
    match serde_json::from_slice::<String>(quoted) {
        Ok(key) => append_key_segment(path, key.as_bytes()),
        // A malformed escape is kept as written, which the full parser then reports.
        Err(_) => {
            path.push(b'[');
            path.extend_from_slice(quoted);
            path.push(b']');
        }
    }
}

/// Appends each part of a dotted key such as `package.rust-version`, as
/// [`append_key_segment`] would.
pub(crate) fn append_dotted_key_segments(path: &mut Vec<u8>, dotted: &[u8]) {
    for key in dotted.split(|&byte| byte == b'.') {
        append_key_segment(path, key);
    }
}

/// Whether `byte` may start an identifier key: an ASCII letter or `_`.
#[inline(always)]
fn is_identifier_start(byte: u8) -> bool {
    ((byte | 0x20).wrapping_sub(b'a') < 26) | (byte == b'_')
}

/// Whether `byte` may continue an identifier key: an ASCII letter, digit, or `_`. Written with
/// `|` so loops over it vectorize.
#[inline(always)]
fn is_identifier_byte(byte: u8) -> bool {
    is_identifier_start(byte) | (byte.wrapping_sub(b'0') < 10)
}

/// Whether `key` matches `[A-Za-z_][A-Za-z0-9_]*`.
#[inline]
fn is_identifier_key(key: &[u8]) -> bool {
    key.first().is_some_and(|&first| is_identifier_start(first))
        && key.iter().all(|&byte| is_identifier_byte(byte))
}

/// Like [`is_identifier_key`] for `bytes[start..end]`, but a key of up to 16 bytes is checked
/// as the 16-byte window from `start`: each byte sets a bit when it can't be in an identifier,
/// which compiles to a few vector instructions and one branch, and the bits past the key are
/// masked off.
#[inline(always)]
fn is_identifier_span(bytes: &[u8], start: usize, end: usize) -> bool {
    const WINDOW: usize = 16;
    let len = end - start;
    match bytes.get(start..).and_then(<[u8]>::first_chunk::<WINDOW>) {
        Some(window) if len > 0 && len <= WINDOW => {
            let mut invalid = 0_u16;
            for (lane, &byte) in window.iter().enumerate() {
                invalid |= u16::from(!is_identifier_byte(byte)) << lane;
            }
            let in_key = (1_u32 << len).wrapping_sub(1) as u16;
            invalid & in_key == 0 && is_identifier_start(window[0])
        }
        _ => is_identifier_key(&bytes[start..end]),
    }
}

/// Like [`is_identifier_span`], but `bytes[start..end]` may also be identifiers joined by
/// single dots, such as `payload.user.name`. Longer keys take the segment-by-segment path.
#[inline(always)]
fn is_dotted_identifier_span(bytes: &[u8], start: usize, end: usize) -> bool {
    const WINDOW: usize = 16;
    let len = end - start;
    let Some(window) = bytes.get(start..).and_then(<[u8]>::first_chunk::<WINDOW>) else {
        return false;
    };
    if len == 0 || len > WINDOW {
        return false;
    }
    let (mut invalid, mut dots, mut starts) = (0_u16, 0_u16, 0_u16);
    for (lane, &byte) in window.iter().enumerate() {
        let dot = byte == b'.';
        invalid |= u16::from(!is_identifier_byte(byte) & !dot) << lane;
        dots |= u16::from(dot) << lane;
        starts |= u16::from(is_identifier_start(byte)) << lane;
    }
    let in_key = (1_u32 << len).wrapping_sub(1) as u16;
    // Each part's first byte, the key's own and each after a dot, must start an identifier, and
    // the key can't end with a dot, which would leave an empty last part.
    let heads = ((dots << 1) | 1) & in_key;
    invalid & in_key == 0 && heads & !starts == 0 && dots & (1 << (len - 1)) == 0
}

/// Whether a JSON string must escape `byte`.
#[inline(always)]
fn needs_escape(byte: u8) -> bool {
    byte < 0x20 || byte == b'"' || byte == b'\\'
}

/// Appends `key` with the escapes `serde_json` writes: short forms where JSON has them, and
/// `\u00XX` for other control characters.
fn append_escaped(path: &mut Vec<u8>, key: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &byte in key {
        match byte {
            b'"' => path.extend_from_slice(br#"\""#),
            b'\\' => path.extend_from_slice(br"\\"),
            b'\n' => path.extend_from_slice(br"\n"),
            b'\r' => path.extend_from_slice(br"\r"),
            b'\t' => path.extend_from_slice(br"\t"),
            0x08 => path.extend_from_slice(br"\b"),
            0x0c => path.extend_from_slice(br"\f"),
            0..0x20 => {
                path.extend_from_slice(br"\u00");
                path.push(HEX[usize::from(byte >> 4)]);
                path.push(HEX[usize::from(byte & 0xf)]);
            }
            _ => path.push(byte),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_path(key: &str) -> String {
        let mut path = b"$".to_vec();
        append_key_segment(&mut path, key.as_bytes());
        String::from_utf8(path).expect("paths are UTF-8")
    }

    fn json_key_path(json_string: &str) -> String {
        let mut path = b"$".to_vec();
        append_json_key_segment(&mut path, json_string.as_bytes(), 1, json_string.len() - 1);
        String::from_utf8(path).expect("paths are UTF-8")
    }

    #[test]
    fn identifiers_are_dotted_and_other_keys_are_quoted() {
        assert_eq!(key_path("e2e"), "$.e2e");
        assert_eq!(key_path("_private"), "$._private");
        assert_eq!(key_path("my-job"), r#"$["my-job"]"#);
        assert_eq!(key_path("123"), r#"$["123"]"#);
        assert_eq!(key_path("a.b"), r#"$["a.b"]"#);
        assert_eq!(key_path(""), r#"$[""]"#);
        assert_eq!(key_path("say \"hi\""), r#"$["say \"hi\""]"#);
        assert_eq!(key_path("caf\u{e9}"), "$[\"caf\u{e9}\"]");
    }

    #[test]
    fn escapes_match_serde_json() {
        for key in [
            "tab\there",
            "line\nbreak",
            "bell\u{7}",
            "nul\u{0}",
            "q\"b\\s",
            "\u{1f}\u{c}\u{8}",
        ] {
            let expected = format!(
                "$[{}]",
                serde_json::to_string(key).expect("strings serialize")
            );
            assert_eq!(key_path(key), expected, "{key:?}");
        }
    }

    #[test]
    fn json_keys_print_like_decoded_keys() {
        for (json_string, key) in [
            (r#""e2e""#, "e2e"),
            (r#""my-job""#, "my-job"),
            (r#""123""#, "123"),
            (r#""q\"t""#, "q\"t"),
            (r#""ABC""#, "ABC"),
            (r#""a\/b""#, "a/b"),
        ] {
            assert_eq!(json_key_path(json_string), key_path(key), "{json_string}");
        }
    }

    #[test]
    fn windowed_identifier_checks_match_the_byte_loop() {
        let document = b"e2e my-job 123 _x Az09_ a.b sixteen_bytes_ab seventeen_bytes_ab x\"y   ";
        let mut start = 0;
        for word in document
            .split(|&byte| byte == b' ')
            .filter(|word| !word.is_empty())
        {
            start += document[start..]
                .windows(word.len())
                .position(|window| window == word)
                .expect("each word is in the document");
            let end = start + word.len();
            assert_eq!(
                is_identifier_span(document, start, end),
                is_identifier_key(word),
                "{}",
                String::from_utf8_lossy(word)
            );
            start = end;
        }
    }

    #[test]
    fn windowed_dotted_checks_accept_only_identifier_parts() {
        let dotted = |key: &str| {
            let mut padded = key.as_bytes().to_vec();
            padded.resize(32, b' ');
            is_dotted_identifier_span(&padded, 0, key.len())
        };
        for key in ["a", "service.name", "payload.user", "_a.b_2"] {
            assert!(dotted(key), "{key}");
        }
        // Keys longer than the window take the segment-by-segment path instead.
        assert!(!dotted("payload.user.tier"));
        for key in [
            "",
            "a.",
            ".a",
            "a..b",
            "a.1b",
            "1a",
            "a.my-key",
            "a b",
            "sixteen.bytes.abc",
        ] {
            assert!(!dotted(key), "{key}");
        }
    }

    #[test]
    fn dotted_keys_split_into_segments() {
        let mut path = b"$".to_vec();
        append_dotted_key_segments(&mut path, b"package.rust-version");
        assert_eq!(path, br#"$.package["rust-version"]"#);
    }

    #[test]
    fn array_segments_preserve_the_parent_path() {
        let mut path = b"$.items".to_vec();
        append_array_segment(&mut path, 0);
        append_array_segment(&mut path, usize::MAX);
        assert_eq!(path, format!("$.items[0][{}]", usize::MAX).as_bytes());
    }
}
