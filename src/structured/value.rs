use std::borrow::Cow;
use std::collections::HashSet;

use json_spanned_value::spanned as json_spanned;
use libyaml_safer::{
    Document as YamlDocument, Node as YamlNode, NodeData as YamlNodeData, NodePair as YamlNodePair,
    Parser as YamlParser, ScalarStyle as YamlScalarStyle,
};
use smallvec::SmallVec;
use toml_spanner::{
    ArrayStyle as TomlArrayStyle, Item as TomlItem, TableStyle as TomlTableStyle,
    Value as TomlValue,
};

use crate::structured_predicate::Number;
use crate::{SearchError, StructuredMode};

pub(super) fn json_scalar_text<'a>(
    node: &'a json_spanned::Value,
    raw_text: &'a str,
) -> Option<Cow<'a, str>> {
    match node.get_ref() {
        json_spanned_value::Value::String(value) => Some(Cow::Borrowed(value.as_str())),
        json_spanned_value::Value::Number(_)
        | json_spanned_value::Value::Bool(_)
        | json_spanned_value::Value::Null => Some(Cow::Borrowed(raw_text)),
        json_spanned_value::Value::Array(_) | json_spanned_value::Value::Object(_) => None,
    }
}

pub(super) fn load_yaml_document(
    parser: &mut YamlParser<&mut &[u8]>,
) -> Result<YamlDocument, SearchError> {
    let document = YamlDocument::load(parser).map_err(|error| SearchError::StructuredParse {
        mode: StructuredMode::Yaml,
        message: error.to_string(),
    })?;
    if let Some(fault) = yaml_alias_fault(&document) {
        return Err(SearchError::StructuredParse {
            mode: StructuredMode::Yaml,
            message: fault.to_string(),
        });
    }
    Ok(document)
}

/// Deepest YAML nesting searched, counting the scalar's level. The visit recurses per level,
/// so this bounds stack use; the JSON and TOML parsers bound their own depth.
const MAX_NESTING: usize = 512;
/// Most values aliases may expand to, as a multiple of the document's node count. This stops
/// "billion laughs" documents, where nested aliases expand exponentially.
const MAX_ALIAS_EXPANSION: usize = 64;
/// Minimum expanded value budget for any document.
const MIN_EXPANDED_VALUES: usize = 1 << 16;

/// Returns why `document` cannot be searched: a cyclic alias, alias expansion beyond
/// [`MAX_ALIAS_EXPANSION`], or nesting beyond [`MAX_NESTING`] with aliases followed.
fn yaml_alias_fault(document: &YamlDocument) -> Option<&'static str> {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Visit {
        Unseen,
        /// On the current root-to-node path.
        Open,
        Done(Extent),
    }

    /// The values a node expands to (itself included) and the nesting levels they span.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Extent {
        values: usize,
        levels: usize,
    }

    /// A node on the current path, its next child position, and its extent so far.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Frame {
        node: usize,
        position: usize,
        extent: Extent,
    }

    impl Frame {
        fn new(node: usize) -> Self {
            Self {
                node,
                position: 0,
                extent: Extent {
                    values: 1,
                    levels: 1,
                },
            }
        }

        fn include(&mut self, child: Extent) {
            self.extent.values = self.extent.values.saturating_add(child.values);
            self.extent.levels = self.extent.levels.max(child.levels + 1);
        }
    }

    /// The index of `node`'s `position`-th child in the document's node list.
    fn child(node: &YamlNode, position: usize) -> Option<usize> {
        let id = match &node.data {
            YamlNodeData::Sequence { items, .. } => *items.get(position)?,
            YamlNodeData::Mapping { pairs, .. } => pairs.get(position)?.value,
            YamlNodeData::Scalar { .. } | YamlNodeData::NoNode => return None,
        };
        // Node ids count from one.
        usize::try_from(id).ok()?.checked_sub(1)
    }

    const TOO_DEEP: &str = "values are nested too deeply";

    let nodes = &document.nodes;
    let most = nodes
        .len()
        .saturating_mul(MAX_ALIAS_EXPANSION)
        .max(MIN_EXPANDED_VALUES);
    let mut visits = vec![Visit::Unseen; nodes.len()];
    // An explicit stack, since recursion could overflow on deep documents.
    let mut path = Vec::from_iter((!nodes.is_empty()).then(|| Frame::new(0)));
    visits
        .iter_mut()
        .take(1)
        .for_each(|root| *root = Visit::Open);
    while let Some(&Frame {
        node,
        position,
        extent,
    }) = path.last()
    {
        let depth = path.len();
        let Some(next) = child(&nodes[node], position) else {
            visits[node] = Visit::Done(extent);
            path.pop();
            match path.last_mut() {
                Some(parent) => parent.include(extent),
                None if extent.values > most => return Some("aliases stand for too many values"),
                None => {}
            }
            continue;
        };
        path[depth - 1].position += 1;
        match visits.get(next) {
            Some(Visit::Open) => return Some("an alias refers to a collection that contains it"),
            Some(Visit::Unseen) if depth == MAX_NESTING => return Some(TOO_DEEP),
            Some(Visit::Unseen) => {
                visits[next] = Visit::Open;
                path.push(Frame::new(next));
            }
            // An alias to a finished node hangs all of its levels below this one.
            Some(&Visit::Done(below)) if depth + below.levels > MAX_NESTING => {
                return Some(TOO_DEEP);
            }
            Some(&Visit::Done(below)) => path[depth - 1].include(below),
            None => {}
        }
    }
    None
}

pub(super) fn yaml_document_scalar_text<'a>(
    node: &'a YamlNode,
    raw_text: &'a str,
) -> Option<Cow<'a, str>> {
    let YamlNodeData::Scalar { value, style } = &node.data else {
        return None;
    };

    match classify_yaml_plain_scalar(raw_text, *style) {
        PlainYamlScalarKind::Null => Some(Cow::Borrowed("null")),
        PlainYamlScalarKind::Bool(value) => {
            Some(Cow::Borrowed(if value { "true" } else { "false" }))
        }
        PlainYamlScalarKind::Number => Some(Cow::Borrowed(raw_text)),
        PlainYamlScalarKind::String => Some(Cow::Borrowed(value.as_str())),
    }
}

/// Names a mapping key by its text whatever type it resolves to, as patterns spell it, so
/// `123:` and `on:` are `123` and `on`. A complex key is named by its source text.
pub(super) fn yaml_document_key_text<'a>(
    document: &'a YamlDocument,
    key_id: i32,
    haystack: &'a str,
) -> &'a str {
    let Some(key) = document.get_node(key_id) else {
        return "";
    };
    match &key.data {
        YamlNodeData::Scalar { value, .. } => value.as_str(),
        YamlNodeData::Sequence { .. } | YamlNodeData::Mapping { .. } | YamlNodeData::NoNode => {
            yaml_document_byte_range(key).map_or("", |(start, end)| &haystack[start..end])
        }
    }
}

/// Whether a mapping key is YAML's merge key, a plain `<<`.
pub(super) fn is_yaml_merge_key(document: &YamlDocument, key_id: i32) -> bool {
    matches!(
        document.get_node(key_id).map(|key| &key.data),
        Some(YamlNodeData::Scalar { value, style: YamlScalarStyle::Plain }) if value == "<<"
    )
}

/// The entries of a YAML mapping as `(key text, value)`, with merge keys applied as YAML 1.1
/// defines them: a merged mapping adds only the keys the mapping lacks, and of the mappings
/// in a merged sequence, earlier ones win.
pub(super) fn yaml_mapping_entries<'a>(
    document: &'a YamlDocument,
    pairs: &'a [YamlNodePair],
    haystack: &'a str,
) -> impl Iterator<Item = (&'a str, &'a YamlNode)> {
    let merged = pairs
        .iter()
        .any(|pair| is_yaml_merge_key(document, pair.key))
        .then(|| merged_yaml_entries(document, pairs, haystack));
    let own = merged
        .is_none()
        .then(|| own_yaml_entries(document, pairs, haystack));
    own.into_iter()
        .flatten()
        .chain(merged.into_iter().flatten())
}

fn own_yaml_entries<'a>(
    document: &'a YamlDocument,
    pairs: &'a [YamlNodePair],
    haystack: &'a str,
) -> impl Iterator<Item = (&'a str, &'a YamlNode)> {
    pairs.iter().filter_map(move |pair| {
        let value = document.get_node(pair.value)?;
        Some((yaml_document_key_text(document, pair.key, haystack), value))
    })
}

#[cold]
#[inline(never)]
fn merged_yaml_entries<'a>(
    document: &'a YamlDocument,
    pairs: &'a [YamlNodePair],
    haystack: &'a str,
) -> Vec<(&'a str, &'a YamlNode)> {
    let mut entries = Vec::with_capacity(pairs.len());
    let mut seen = HashSet::with_capacity(pairs.len());
    collect_yaml_entries(document, pairs, haystack, (&mut entries, &mut seen));
    entries
}

/// Adds the entries of a mapping whose keys aren't `seen` yet: its own, then those of each
/// mapping it merges, in order. One pass over the merged mappings keeps chains of merges
/// linear.
fn collect_yaml_entries<'a>(
    document: &'a YamlDocument,
    pairs: &'a [YamlNodePair],
    haystack: &'a str,
    (entries, seen): (&mut Vec<(&'a str, &'a YamlNode)>, &mut HashSet<&'a str>),
) {
    let is_merge = |pair: &&YamlNodePair| is_yaml_merge_key(document, pair.key);
    for pair in pairs.iter().filter(|pair| !is_merge(pair)) {
        let key = yaml_document_key_text(document, pair.key, haystack);
        if let Some(value) = document.get_node(pair.value)
            && seen.insert(key)
        {
            entries.push((key, value));
        }
    }
    for pair in pairs.iter().filter(is_merge) {
        let sources = match document.get_node(pair.value).map(|value| &value.data) {
            Some(YamlNodeData::Sequence { items, .. }) => items.as_slice(),
            _ => std::slice::from_ref(&pair.value),
        };
        for source in sources.iter().filter_map(|&id| document.get_node(id)) {
            if let YamlNodeData::Mapping { pairs, .. } = &source.data {
                collect_yaml_entries(document, pairs, haystack, (&mut *entries, &mut *seen));
            }
        }
    }
}

pub(super) fn yaml_document_byte_range(node: &YamlNode) -> Option<(usize, usize)> {
    let start = usize::try_from(node.start_mark.index).ok()?;
    let end = usize::try_from(node.end_mark.index).ok()?;
    (start <= end).then_some((start, end))
}

#[derive(Clone, Copy)]
pub(super) enum PlainYamlScalarKind {
    Null,
    Bool(bool),
    Number,
    String,
}

pub(super) fn classify_yaml_plain_scalar(
    raw_text: &str,
    style: YamlScalarStyle,
) -> PlainYamlScalarKind {
    if style != YamlScalarStyle::Plain {
        return PlainYamlScalarKind::String;
    }

    // Every keyword below is at most five bytes and starts with one of these bytes.
    let maybe_keyword = raw_text.len() <= 5
        && matches!(
            raw_text.as_bytes().first(),
            Some(b'~' | b'n' | b'N' | b't' | b'T' | b'y' | b'Y' | b'o' | b'O' | b'f' | b'F')
        );
    if maybe_keyword {
        if matches!(raw_text, "~" | "null" | "Null" | "NULL") {
            return PlainYamlScalarKind::Null;
        }
        if matches!(
            raw_text,
            "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON"
        ) {
            return PlainYamlScalarKind::Bool(true);
        }
        if matches!(
            raw_text,
            "false" | "False" | "FALSE" | "no" | "No" | "NO" | "off" | "Off" | "OFF"
        ) {
            return PlainYamlScalarKind::Bool(false);
        }
    }

    if looks_like_yaml_number(raw_text) {
        return PlainYamlScalarKind::Number;
    }

    PlainYamlScalarKind::String
}

/// Whether a plain scalar is a YAML 1.1 int or float: decimal with optional fraction and
/// exponent, `0x`/`0o`/`0b` radix, or base 60 such as `1:30`, with `_` between digits. Text
/// that only uses those characters, such as `e2e` or `2.4.1`, stays a string. `.inf`,
/// `-.inf`, and `.nan` are floats too.
#[inline]
fn looks_like_yaml_number(raw_text: &str) -> bool {
    let bytes = raw_text.as_bytes();
    // A number starts with a digit, a sign, or a point, which rules out most strings at once,
    // and most numbers are plain integers. Only the rest take the full grammar.
    matches!(bytes.first(), Some(b'0'..=b'9' | b'+' | b'-' | b'.'))
        && (bytes.iter().all(u8::is_ascii_digit) || follows_yaml_number_grammar(raw_text))
}

/// The full grammar of [`looks_like_yaml_number`], out of line from the common cases.
#[inline(never)]
fn follows_yaml_number_grammar(raw_text: &str) -> bool {
    if yaml_special_float(raw_text).is_some() {
        return true;
    }
    // Numbers only spell these bytes, which rules out text such as timestamps.
    let spells_number = raw_text.bytes().all(|byte| {
        matches!(
            byte,
            b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' | b'o' | b'O' | b'x' | b'X' | b'+' | b'-'
                | b'.' | b'_' | b':'
        )
    });
    if !spells_number {
        return false;
    }
    let unsigned = raw_text.strip_prefix(['+', '-']).unwrap_or(raw_text);
    let radix_digits = |digits: &[u8], is_digit: fn(&u8) -> bool| {
        is_digit_run(digits, is_digit) && digits.len() == digit_run_len(digits, is_digit)
    };
    match unsigned.as_bytes() {
        [b'0', b'x' | b'X', digits @ ..] => radix_digits(digits, u8::is_ascii_hexdigit),
        [b'0', b'o' | b'O', digits @ ..] => {
            radix_digits(digits, |byte| matches!(byte, b'0'..=b'7'))
        }
        [b'0', b'b' | b'B', digits @ ..] => {
            radix_digits(digits, |byte| matches!(byte, b'0' | b'1'))
        }
        bytes => is_yaml_decimal(bytes) || is_yaml_base_60(bytes),
    }
}

/// The value of YAML's `.inf`, `-.inf`, or `.nan`, in any of their three spellings.
pub(super) fn yaml_special_float(raw_text: &str) -> Option<f64> {
    let (sign, unsigned) = match raw_text.as_bytes().first() {
        Some(b'-') => (-1.0, &raw_text[1..]),
        Some(b'+') => (1.0, &raw_text[1..]),
        _ => (1.0, raw_text),
    };
    match unsigned {
        ".inf" | ".Inf" | ".INF" => Some(sign * f64::INFINITY),
        ".nan" | ".NaN" | ".NAN" if unsigned.len() == raw_text.len() => Some(f64::NAN),
        _ => None,
    }
}

/// Whether `bytes` starts with a digit, so that `_` separators may follow it.
fn is_digit_run(bytes: &[u8], is_digit: fn(&u8) -> bool) -> bool {
    bytes.first().is_some_and(is_digit)
}

/// The length of the leading digits and `_` separators of `bytes`.
fn digit_run_len(bytes: &[u8], is_digit: fn(&u8) -> bool) -> usize {
    bytes
        .iter()
        .position(|byte| !is_digit(byte) && *byte != b'_')
        .unwrap_or(bytes.len())
}

/// `([0-9][0-9_]*)?(\.[0-9_]*)?([eE][-+]?[0-9]+)?` with at least one digit before the exponent.
fn is_yaml_decimal(bytes: &[u8]) -> bool {
    let integer = digit_run_len(bytes, u8::is_ascii_digit);
    if integer > 0 && !is_digit_run(bytes, u8::is_ascii_digit) {
        return false;
    }
    let mut rest = &bytes[integer..];
    let mut has_digit = integer > 0;
    if let Some(fraction) = rest.strip_prefix(b".") {
        let length = digit_run_len(fraction, u8::is_ascii_digit);
        has_digit |= fraction[..length].iter().any(u8::is_ascii_digit);
        rest = &fraction[length..];
    }
    let exponent = match rest {
        [] => return has_digit,
        [b'e' | b'E', b'+' | b'-', exponent @ ..] | [b'e' | b'E', exponent @ ..] => exponent,
        _ => return false,
    };
    has_digit && !exponent.is_empty() && exponent.iter().all(u8::is_ascii_digit)
}

/// `[1-9][0-9_]*(:[0-5]?[0-9])+(\.[0-9_]*)?`, YAML 1.1's base 60 such as `1:30:00`.
fn is_yaml_base_60(bytes: &[u8]) -> bool {
    if !matches!(bytes.first(), Some(b'1'..=b'9')) {
        return false;
    }
    let (whole, fraction) = match bytes.iter().position(|&byte| byte == b'.') {
        Some(dot) => (&bytes[..dot], &bytes[dot + 1..]),
        None => (bytes, &[][..]),
    };
    let mut parts = whole.split(|&byte| byte == b':');
    let hours = parts.next().unwrap_or_default();
    let mut sexagesimal = parts.peekable();
    sexagesimal.peek().is_some()
        && hours.len() == digit_run_len(hours, u8::is_ascii_digit)
        && sexagesimal.all(|part| match part {
            [digit] => digit.is_ascii_digit(),
            [b'0'..=b'5', digit] => digit.is_ascii_digit(),
            _ => false,
        })
        && fraction.len() == digit_run_len(fraction, u8::is_ascii_digit)
}

/// The value of a YAML 1.1 base 60 number without `_` separators, such as `1:30` (90) or
/// `-1:30.5` (-90.5).
pub(super) fn yaml_base_60(text: &str) -> Option<Number> {
    let (negative, unsigned) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if !is_yaml_base_60(unsigned.as_bytes()) {
        return None;
    }
    let (whole, fraction) = unsigned
        .find('.')
        .map_or((unsigned, ""), |dot| unsigned.split_at(dot));
    let magnitude = whole.split(':').try_fold(0_i64, |total, part| {
        total.checked_mul(60)?.checked_add(part.parse().ok()?)
    })?;
    let sign = if negative { -1 } else { 1 };
    Some(match fraction {
        "" => Number::Integer(sign * magnitude),
        "." => Number::Float((sign * magnitude) as f64),
        _ => Number::Float(sign as f64 * (magnitude as f64 + fraction.parse::<f64>().ok()?)),
    })
}

/// The span of a TOML value as written, which reads as TOML on its own. Tables written with
/// headers widen over the subtables later headers add, as `[server]` does over
/// `[server.tls]`. A table made by dotted keys or implied by a deeper header is spanned by its
/// key alone, so it also widens back to the start of its dotted key: `b` in `a.b.c = 1` spans
/// `a.b.c = 1`, and `x` in `[x.y]` spans that header's table.
pub(super) fn toml_item_span(item: &TomlItem<'_>, haystack: &str) -> (usize, usize) {
    // Dotted keys and headers nest tables without limit, so descendants wait on the heap
    // rather than in recursive calls. Each waits with its parent's key part and the start of
    // the dotted key holding it, so a chain of tables in one key is walked back once.
    let mut pending = SmallVec::<[(&TomlItem<'_>, Option<KeyPart>); 8]>::new();
    pending.push((item, None));
    let mut span = (usize::MAX, 0);
    while let Some((item, parent)) = pending.pop() {
        let own = item.span();
        let (start, end) = (own.start as usize, own.end as usize);
        let start = match item.value() {
            TomlValue::Table(table) => {
                let key_start = match table.style() {
                    TomlTableStyle::Inline => {
                        span = (span.0.min(start), span.1.max(end));
                        continue;
                    }
                    TomlTableStyle::Header => start,
                    TomlTableStyle::Dotted | TomlTableStyle::Implicit => {
                        dotted_key_start(haystack.as_bytes(), start, parent)
                    }
                };
                let part = Some(KeyPart { start, key_start });
                pending.extend(table.entries().iter().map(|(_, child)| (child, part)));
                key_start
            }
            TomlValue::Array(array) if array.style() == TomlArrayStyle::Header => {
                pending.extend(array.iter().map(|child| (child, None)));
                start
            }
            TomlValue::String(_)
            | TomlValue::Integer(_)
            | TomlValue::Float(_)
            | TomlValue::Boolean(_)
            | TomlValue::DateTime(_)
            | TomlValue::Array(_) => start,
        };
        span = (span.0.min(start), span.1.max(end));
    }
    span
}

/// A part of a TOML key, with the start of the dotted key holding it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct KeyPart {
    start: usize,
    key_start: usize,
}

/// The start of the dotted key whose part starts at `part_start`, such as `a` for `c` in
/// `a.b.c`. Returns `part_start` when no `.` precedes it. Reaching `known`'s part ends the
/// walk at its key's start.
fn dotted_key_start(bytes: &[u8], part_start: usize, known: Option<KeyPart>) -> usize {
    let skip_blanks = |end: usize| {
        bytes[..end]
            .iter()
            .rposition(|&byte| !matches!(byte, b' ' | b'\t'))
            .map_or(0, |last| last + 1)
    };
    let mut start = part_start;
    loop {
        if let Some(known) = known.filter(|known| known.start == start) {
            return known.key_start;
        }
        let dot = skip_blanks(start);
        if dot == 0 || bytes[dot - 1] != b'.' {
            return start;
        }
        let key_end = skip_blanks(dot - 1);
        let previous = match bytes[..key_end].last() {
            Some(&quote @ (b'"' | b'\'')) => opening_quote(bytes, key_end - 1, quote),
            _ => bytes[..key_end]
                .iter()
                .rposition(|&byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
                .map_or(Some(0), |last| Some(last + 1)),
        };
        match previous {
            Some(previous) if previous < key_end => start = previous,
            _ => return start,
        }
    }
}

/// The opening quote of the quoted key whose closing `quote` is at `closing`, skipping quotes
/// a basic string escapes.
fn opening_quote(bytes: &[u8], closing: usize, quote: u8) -> Option<usize> {
    let mut end = closing;
    loop {
        let candidate = memchr::memrchr(quote, &bytes[..end])?;
        let backslashes = bytes[..candidate]
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\\')
            .count();
        if quote == b'\'' || backslashes % 2 == 0 {
            return Some(candidate);
        }
        end = candidate;
    }
}

pub(super) fn toml_scalar_text<'a>(
    item: &'a TomlItem<'_>,
    raw_text: &'a str,
) -> Option<Cow<'a, str>> {
    match item.value() {
        TomlValue::String(value) => Some(Cow::Borrowed(value)),
        TomlValue::Integer(_)
        | TomlValue::Float(_)
        | TomlValue::Boolean(_)
        | TomlValue::DateTime(_) => Some(Cow::Borrowed(raw_text)),
        TomlValue::Array(_) | TomlValue::Table(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alias_fault(yaml: &str) -> Option<&'static str> {
        let mut parser = YamlParser::new();
        let mut bytes = yaml.as_bytes();
        parser.set_input_string(&mut bytes);
        let document = YamlDocument::load(&mut parser).expect("the YAML parses");
        yaml_alias_fault(&document)
    }

    #[test]
    fn yaml_numbers_follow_the_yaml_1_1_grammar() {
        let numbers = [
            "0",
            "-7",
            "+12",
            "1_000",
            "0755",
            "3.12",
            "-.5",
            "1.",
            "6.02e23",
            "1.5E-3",
            "0x1F",
            "0o17",
            "0b1010",
            "1:30",
            "190:20:30.15",
            ".inf",
            "-.Inf",
            "+.INF",
            ".nan",
            ".NaN",
        ];
        let strings = [
            "e2e",
            "b2b",
            "x86",
            "2.4.1",
            "20.x",
            "09:14:31",
            "1:60",
            "_1",
            ".",
            "1e",
            "0x",
            "1.0.0-rc1",
            "inf",
            "nan",
            ".Nan",
            "-.nan",
            "v2",
            "-",
            "1e2e",
        ];

        for number in numbers {
            assert!(looks_like_yaml_number(number), "{number:?} is a number");
        }
        for string in strings {
            assert!(!looks_like_yaml_number(string), "{string:?} is a string");
        }
    }

    #[test]
    fn shared_anchors_are_no_fault() {
        let shared = "a: &x [1, 2]\nb: *x\nc: {d: *x, e: [*x, *x]}\n";

        assert_eq!(alias_fault(shared), None);
        assert_eq!(alias_fault("plain: [1, {two: 3}]\n"), None);
        assert_eq!(alias_fault(""), None);
    }

    #[test]
    fn aliases_to_an_enclosing_collection_are_a_fault() {
        for cyclic in ["&a [*a]\n", "a: &a\n  b: *a\n", "&a\nk: [1, {x: *a}]\n"] {
            assert_eq!(
                alias_fault(cyclic),
                Some("an alias refers to a collection that contains it"),
                "{cyclic:?}"
            );
        }
    }

    #[test]
    fn aliases_of_aliases_that_multiply_values_are_a_fault() {
        // Each level expands tenfold: a billion values in nine lines.
        let mut laughs = String::from("l0: &l0 [x, x, x, x, x, x, x, x, x, x]\n");
        for level in 1..9 {
            let previous = format!("*l{}", level - 1);
            laughs.push_str(&format!(
                "l{level}: &l{level} [{}]\n",
                [previous.as_str(); 10].join(", ")
            ));
        }

        assert_eq!(
            alias_fault(&laughs),
            Some("aliases stand for too many values")
        );
    }

    #[test]
    fn values_nested_too_deeply_are_a_fault_also_through_aliases() {
        let nested = |levels: usize| format!("{}x{}", "[".repeat(levels), "]".repeat(levels));

        // The scalar is a level of its own below the sequences around it.
        assert_eq!(alias_fault(&nested(MAX_NESTING - 1)), None);
        assert_eq!(
            alias_fault(&nested(MAX_NESTING)),
            Some("values are nested too deeply")
        );
        // An alias hangs the levels of its anchor below the levels around the alias.
        let half = nested(MAX_NESTING / 2 - 2);
        let around = |levels: usize| {
            format!(
                "a: &a {half}\nb: {}*a{}\n",
                "[".repeat(levels),
                "]".repeat(levels)
            )
        };
        assert_eq!(alias_fault(&around(MAX_NESTING / 2)), None);
        assert_eq!(
            alias_fault(&around(MAX_NESTING / 2 + 1)),
            Some("values are nested too deeply")
        );
    }

    /// Returns the text `toml_item_span` gives the value at `path`, where numeric segments
    /// index arrays.
    fn written_toml<'a>(document: &'a str, path: &[&str]) -> &'a str {
        let arena = toml_spanner::Arena::new();
        let root = toml_spanner::parse(document, &arena).expect("the TOML parses");
        let item = path
            .iter()
            .try_fold(root.table().as_item(), |item, segment| {
                match segment.parse::<usize>() {
                    Ok(index) => item.as_array()?.get(index),
                    Err(_) => item.as_table()?.get(segment),
                }
            })
            .expect("the value exists");
        let (start, end) = toml_item_span(item, document);
        &document[start..end]
    }

    #[test]
    fn toml_values_span_their_quotes_as_written() {
        let document = concat!(
            "a = \"x\\\"y\"\nb = ''\nc = \"\"\nd = '''z'''\ne = \"\"\"\r\nline\"\"\"\n",
            "f = \"\"\"\"\"\"\ng = [\"\", 'q']\nh = 42\n",
        );
        let written = |path: &[&str]| written_toml(document, path);

        assert_eq!(written(&["a"]), "\"x\\\"y\"");
        assert_eq!(written(&["b"]), "''");
        assert_eq!(written(&["c"]), "\"\"");
        assert_eq!(written(&["d"]), "'''z'''");
        assert_eq!(written(&["e"]), "\"\"\"\r\nline\"\"\"");
        assert_eq!(written(&["f"]), "\"\"\"\"\"\"");
        assert_eq!(written(&["g", "0"]), "\"\"");
        assert_eq!(written(&["g", "1"]), "'q'");
        assert_eq!(written(&["h"]), "42");
    }

    #[test]
    fn toml_datetimes_span_their_text_as_written() {
        let document = concat!(
            "odt = 1979-05-27T07:32:00Z\nspaced = 1979-05-27 07:32:00.999-07:00\n",
            "ldt = 1979-05-27T07:32:00\nld = 1979-05-27\nlt = 07:32:00\n",
            "short = 1979-05-27T07:32Z\nlisted = [1979-05-27, 2]\n",
        );
        let written = |path: &[&str]| written_toml(document, path);

        assert_eq!(written(&["odt"]), "1979-05-27T07:32:00Z");
        assert_eq!(written(&["spaced"]), "1979-05-27 07:32:00.999-07:00");
        assert_eq!(written(&["ldt"]), "1979-05-27T07:32:00");
        assert_eq!(written(&["ld"]), "1979-05-27");
        assert_eq!(written(&["lt"]), "07:32:00");
        assert_eq!(written(&["short"]), "1979-05-27T07:32Z");
        assert_eq!(written(&["listed", "0"]), "1979-05-27");
    }

    #[test]
    fn toml_dotted_and_implied_tables_span_their_entries() {
        let document = concat!(
            "a.b.c = \"x\"\na.d = 1\na.b.e = 2\ninline = { p.q.r = 3 }\n",
            "\"q.x\" . 'r' .s = 4\n\"e\\\"q\".f.g = 5\n\n",
            "[x.y]\nk = 1\n\n[x.z]\nk = 2\n\n[t]\nn = 1\n",
        );
        let written = |path: &[&str]| written_toml(document, path);

        assert_eq!(written(&["a"]), "a.b.c = \"x\"\na.d = 1\na.b.e = 2");
        assert_eq!(written(&["a", "b"]), "a.b.c = \"x\"\na.d = 1\na.b.e = 2");
        assert_eq!(written(&["inline", "p"]), "p.q.r = 3");
        assert_eq!(written(&["inline", "p", "q"]), "p.q.r = 3");
        assert_eq!(written(&["q.x", "r"]), "\"q.x\" . 'r' .s = 4");
        assert_eq!(written(&["e\"q", "f"]), "\"e\\\"q\".f.g = 5");
        assert_eq!(written(&["x"]), "[x.y]\nk = 1\n\n[x.z]\nk = 2");
        assert_eq!(written(&["x", "y"]), "[x.y]\nk = 1");
        assert_eq!(written(&["t"]), "[t]\nn = 1");
    }
}
