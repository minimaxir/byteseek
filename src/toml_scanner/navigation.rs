//! Parser-free lookup of `$.table[i].key` paths into TOML arrays of tables.
//!
//! Only the common layout is handled. Anything ambiguous (indented or quoted headers, multi-line
//! strings, inline arrays and tables) yields `None`, leaving the lookup to the full parser.

use memchr::{memchr_iter, memmem};

use super::{extract_toml_value, find_line_end};
use crate::CaseMode;
use crate::exact::{BLOCK, Literal, adjacent_blocks, flagged_positions};
use crate::json_scanner::PathSegment;

/// A compiled `[[table]]` entry and dotted key path for [`toml_navigate_path`].
#[derive(Debug, Clone)]
pub(crate) struct TomlPath {
    table_name: String,
    /// `\n[[table]]`, the usual spelling of the header.
    plain_header: Literal,
    entry_index: usize,
    /// `key = ` and `key= `, in order of preference.
    key_spellings: [memmem::Finder<'static>; 2],
}

impl TomlPath {
    /// Compiles `[table, index, key, ...]` segments, or returns `None` for other shapes and
    /// for keys that would need quoting.
    pub(crate) fn new(segments: &[PathSegment]) -> Option<Self> {
        let [
            PathSegment::Key(table_name),
            PathSegment::Index(entry_index),
            key_segments @ ..,
        ] = segments
        else {
            return None;
        };
        if !is_bare_key(table_name) || key_segments.is_empty() {
            return None;
        }
        let dotted_key = build_dotted_key(key_segments)?;
        Some(Self {
            plain_header: Literal::new(
                format!("\n[[{table_name}]]").as_bytes(),
                CaseMode::Sensitive,
            ),
            table_name: table_name.clone(),
            entry_index: *entry_index,
            key_spellings: [" = ", "= "].map(|separator| {
                memmem::Finder::new(format!("{dotted_key}{separator}").as_bytes()).into_owned()
            }),
        })
    }
}

/// Returns the `(start, end)` span of the value at `path`, or `None` when this fast lookup
/// cannot tell. `None` does not mean the value is absent.
pub(crate) fn toml_navigate_path(bytes: &[u8], path: &TomlPath) -> Option<(usize, usize)> {
    let table_name = path.table_name.as_str();
    let mut entries_before = 0;
    // One pass over doubled brackets and quotes finds the entry's header, bailing out at
    // multi-line strings, whose lines could look like headers.
    let entry_start = find_doubled(bytes, |position| {
        let plain = position != 0 && path.plain_header.occurs_at(bytes, position - 1, false);
        if !plain {
            let Some(names_table) = header_at(bytes, position, table_name) else {
                // `Some(None)` stops the pass with no entry.
                return Some(None);
            };
            // Moves on to the next mark unless the header names the table.
            names_table.then_some(())?;
        }
        entries_before += 1;
        (entries_before > path.entry_index).then(|| {
            let name_end = position + 2 + table_name.len() + 2;
            Some(find_line_end(bytes, name_end))
        })
    })??;
    // The entry ends at the next header of any table.
    let mut entry_end = bytes.len();
    for position in memchr_iter(b'[', &bytes[entry_start..]) {
        let position = entry_start + position;
        let indent = indent_before(bytes, position);
        let line_start = position - indent;
        if line_start != entry_start && bytes[line_start - 1] != b'\n' {
            continue;
        }
        if indent != 0 {
            return None;
        }
        entry_end = line_start.saturating_sub(1).max(entry_start);
        break;
    }
    let entry = &bytes[entry_start..entry_end];
    // Lines inside a multi-line string could look like keys.
    let long_string = find_doubled(entry, |position| {
        opens_long_string(&entry[position..]).then_some(())
    });
    if long_string.is_some() {
        return None;
    }
    find_dotted_key_value(entry, entry_start, &path.key_spellings)
}

/// Whether the mark at `position` starts a `[[table_name]]` header, or `None` when only a
/// parser can tell (multi-line strings, indented headers, headers with spaces or quotes).
fn header_at(bytes: &[u8], position: usize, table_name: &str) -> Option<bool> {
    let [b'[', b'[', name @ ..] = &bytes[position..] else {
        return (!opens_long_string(&bytes[position..])).then_some(false);
    };
    let indent = indent_before(bytes, position);
    let line_start = position - indent;
    if line_start != 0 && bytes[line_start - 1] != b'\n' {
        // Not at the start of its line, so part of a value or comment.
        return Some(false);
    }
    if indent != 0 {
        return None;
    }
    match name.strip_prefix(table_name.as_bytes()) {
        Some([b']', b']', ..]) => Some(true),
        // Headers with spaces or quotes may name this table too.
        Some([b' ' | b'\t', ..]) => None,
        Some(_) | None => {
            (!matches!(name.first(), Some(b' ' | b'\t' | b'"' | b'\''))).then_some(false)
        }
    }
}

/// Calls `visit` with each position of a doubled `[`, `"` or `'` in ascending order until it
/// returns `Some`, and returns that value.
///
/// Doubled `#` and `&` are reported too: filtering them costs the vector loop an extra step,
/// and `visit` inspects the bytes anyway.
fn find_doubled<T>(bytes: &[u8], mut visit: impl FnMut(usize) -> Option<T>) -> Option<T> {
    let (firsts, seconds, [last_firsts, last_seconds]) = adjacent_blocks(bytes);
    // The partial last block is zero-padded, which never compares as doubled.
    let blocks = firsts
        .iter()
        .zip(seconds)
        .chain([(&last_firsts, &last_seconds)]);
    for (block, (firsts, seconds)) in blocks.enumerate() {
        let mut flags = [0_u8; BLOCK];
        let mut any = 0;
        for ((flag, &first), &second) in flags.iter_mut().zip(firsts).zip(seconds) {
            // Or-ing 0x05 maps `"`, `'`, `#` and `&` to `'`. Non-short-circuit `&` and `|` on
            // `bool` keep the loop branch-free so it vectorizes.
            let quote = (first | 0x05) == b'\'';
            let doubled = (first == second) & (quote | (first == b'['));
            *flag = if doubled { u8::MAX } else { 0 };
            any |= *flag;
        }
        if any == 0 {
            continue;
        }
        for offset in flagged_positions(&flags) {
            if let Some(found) = visit(block * BLOCK + offset) {
                return Some(found);
            }
        }
    }
    None
}

/// Whether `text` starts with `"""` or `'''`.
fn opens_long_string(text: &[u8]) -> bool {
    matches!(text, [b'"', b'"', b'"', ..] | [b'\'', b'\'', b'\'', ..])
}

fn indent_before(bytes: &[u8], position: usize) -> usize {
    bytes[..position]
        .iter()
        .rev()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count()
}

/// Whether `key` is a TOML bare key, which documents write unquoted.
fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Returns the document span of the key's value within one entry, which starts at `offset`.
/// Returns `None` when the key does not start a line or its value is an array or inline table.
fn find_dotted_key_value(
    entry_bytes: &[u8],
    offset: usize,
    key_spellings: &[memmem::Finder<'static>; 2],
) -> Option<(usize, usize)> {
    let starts_line = |pos: &usize| {
        entry_bytes[..*pos]
            .iter()
            .rev()
            .take_while(|&&byte| byte != b'\n')
            .all(|byte| matches!(byte, b' ' | b'\t'))
    };
    let (pos, key) = key_spellings.iter().find_map(|key| {
        let pos = key.find_iter(entry_bytes).find(starts_line)?;
        Some((pos, key))
    })?;
    let (start, end) = extract_toml_value(entry_bytes, pos + key.needle().len());
    // Arrays and inline tables can span lines; leave them to the parser.
    if matches!(entry_bytes.get(start), Some(b'[' | b'{')) {
        return None;
    }
    Some((offset + start, offset + end))
}

/// Joins key segments into a dotted key such as `payload.user.name`.
fn build_dotted_key(segments: &[PathSegment]) -> Option<String> {
    let mut key = String::new();
    for (i, seg) in segments.iter().enumerate() {
        let PathSegment::Key(segment) = seg else {
            return None;
        };
        if !is_bare_key(segment) {
            return None;
        }
        if i > 0 {
            key.push('.');
        }
        key.push_str(segment);
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Navigates `$.events[index].key` in `document`.
    fn navigated<'a>(document: &'a str, index: usize, key: &str) -> Option<&'a str> {
        let segments = [
            PathSegment::Key("events".to_string()),
            PathSegment::Index(index),
            PathSegment::Key(key.to_string()),
        ];
        let path = TomlPath::new(&segments).expect("the path leads into an array of tables");
        toml_navigate_path(document.as_bytes(), &path).map(|(start, end)| &document[start..end])
    }

    #[test]
    fn doubled_brackets_and_quotes_are_found_in_inputs_of_every_length() {
        let text =
            b"[[a]] x = \"\" [b] 'c' '' ## && \xFF\xFF k = [[1], [2]] \"\"\" ss 00 ".repeat(5);
        for len in 0..text.len() {
            let bytes = &text[..len];
            let expected = (0..len.saturating_sub(1))
                .filter(|&position| {
                    bytes[position] == bytes[position + 1] && b"[\"'#&".contains(&bytes[position])
                })
                .collect::<Vec<_>>();

            let mut positions = Vec::new();
            let ended: Option<()> = find_doubled(bytes, |position| {
                positions.push(position);
                None
            });
            let first = find_doubled(bytes, Some);

            assert_eq!((&positions, ended), (&expected, None), "{len}");
            assert_eq!(first, expected.first().copied(), "{len}");
        }
    }

    #[test]
    fn only_brackets_quotes_and_the_two_bytes_alike_are_found_doubled() {
        let found = (0..=u8::MAX)
            .filter(|&byte| find_doubled(&[b'x', byte, byte, b'x'], Some) == Some(1))
            .collect::<Vec<_>>();
        let found_anything_else = (0..=u8::MAX)
            .any(|byte| find_doubled(&[b'x', byte, byte, b'x'], Some).is_some_and(|at| at != 1));

        assert_eq!(found, *b"\"#&'[");
        assert!(!found_anything_else);
    }

    #[test]
    fn paths_compile_only_into_arrays_of_tables_under_bare_keys() {
        let key = |name: &str| PathSegment::Key(name.to_string());
        let compiled = |segments: &[PathSegment]| TomlPath::new(segments).is_some();

        assert!(compiled(&[key("events"), PathSegment::Index(3), key("id")]));
        assert!(compiled(&[
            key("events"),
            PathSegment::Index(0),
            key("a"),
            key("b-c_1")
        ]));
        assert!(!compiled(&[key("events"), PathSegment::Index(3)]));
        assert!(!compiled(&[key("events"), key("id")]));
        assert!(!compiled(&[PathSegment::Index(0), key("id")]));
        assert!(!compiled(&[
            key("my events"),
            PathSegment::Index(0),
            key("id")
        ]));
        assert!(!compiled(&[
            key("events"),
            PathSegment::Index(0),
            key("a.b")
        ]));
        assert!(!compiled(&[
            key("events"),
            PathSegment::Index(0),
            PathSegment::Index(1)
        ]));
    }

    #[test]
    fn values_are_found_in_the_lines_of_the_entry_that_the_index_names() {
        let document = "## notes && more\ntitle = \"\"\n[[events2]]\nid = 0\n[[events]]\nid = 1\n\
                        [[events]]\n[[events]]\nid= 3\nid = 4\n[meta]\nid = 9\n[[events]]";

        let found = [0, 1, 2, 3, 4].map(|index| navigated(document, index, "id"));

        // Entry 1 is empty, entry 2 prefers `id = `, and entry 3 is a bare trailing header.
        assert_eq!(found, [Some("1"), None, Some("4"), None, None]);
    }

    #[test]
    fn values_are_found_after_line_ends_of_either_kind() {
        let document = "[[events]]\r\nid = 5\r\n[[events]]\r\nname = \"b\"\r\n";

        let found = (navigated(document, 0, "id"), navigated(document, 1, "name"));

        assert_eq!(found, (Some("5"), Some("\"b\"")));
    }

    #[test]
    fn documents_that_only_a_parser_reads_are_left_to_it() {
        let indented_header = "[[events]]\n  [[events.parts]]\nid = 1\n";
        let spaced_header = "[[ events ]]\nid = 1\n";
        let long_string_before = "note = \"\"\"\n[[events]]\n\"\"\"\n[[events]]\nid = 1\n";
        let long_string_inside = "[[events]]\nnote = '''\nid = 2\n'''\nid = 1\n";
        let array = "[[events]]\nid = [1, 2]\n";

        let found = [
            indented_header,
            spaced_header,
            long_string_before,
            long_string_inside,
            array,
        ]
        .map(|document| navigated(document, 0, "id"));

        assert_eq!(found, [None; 5]);
    }
}
