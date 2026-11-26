use std::borrow::Cow;

use json_spanned_value::{Value as JsonValueInner, spanned as json_spanned};
use libyaml_safer::{
    Document as YamlDocument, Node as YamlNode, NodeData as YamlNodeData,
    ScalarStyle as YamlScalarStyle,
};
use toml_spanner::{Item as TomlItem, Value as TomlValue};

use super::value::{
    PlainYamlScalarKind, classify_yaml_plain_scalar, is_yaml_merge_key, yaml_base_60,
    yaml_document_byte_range, yaml_document_key_text, yaml_mapping_entries, yaml_special_float,
};
use super::{CompiledPredicatePattern, PathSegment};
use crate::structured_predicate::{Number, PredicateTypeError, PredicateValue, evaluate_clause};

pub(super) struct ResolvedPredicateScalar<'value> {
    start: usize,
    end: usize,
    value: PredicateValue<'value>,
}

pub(super) trait PredicateResolver {
    fn resolve<'value>(
        &'value self,
        segments: &[PathSegment],
    ) -> Result<Option<ResolvedPredicateScalar<'value>>, PredicateTypeError>;
}

type PredicateEvaluationError<'a> = (&'a [PathSegment], PredicateTypeError);

pub(super) fn evaluate_compiled_predicate<'pattern>(
    resolver: &impl PredicateResolver,
    predicate: &'pattern CompiledPredicatePattern,
) -> Result<Option<(usize, usize)>, PredicateEvaluationError<'pattern>> {
    let mut anchor = None;
    for clause in &predicate.clauses {
        let resolved = resolver
            .resolve(&clause.segments)
            .map_err(|error| (clause.segments.as_slice(), error))?;
        let Some(resolved) = resolved else {
            return Ok(None);
        };
        if !evaluate_clause(resolved.value, &clause.operator)
            .map_err(|error| (clause.segments.as_slice(), error))?
        {
            return Ok(None);
        }
        anchor.get_or_insert((resolved.start, resolved.end));
    }
    Ok(anchor)
}

pub(super) trait PredicateNode {
    type Context: ?Sized;

    fn child<'node>(
        &'node self,
        context: &'node Self::Context,
        segment: &PathSegment,
        haystack: &'node str,
    ) -> Result<Option<&'node Self>, PredicateTypeError>;

    fn scalar<'node>(
        &'node self,
        haystack: &'node str,
    ) -> Result<ResolvedPredicateScalar<'node>, PredicateTypeError>;
}

pub(super) struct AstPredicateResolver<'a, Node: PredicateNode> {
    context: &'a Node::Context,
    node: &'a Node,
    haystack: &'a str,
}

impl<'a, Node: PredicateNode> AstPredicateResolver<'a, Node> {
    pub(super) fn new(context: &'a Node::Context, node: &'a Node, haystack: &'a str) -> Self {
        Self {
            context,
            node,
            haystack,
        }
    }
}

impl<Node: PredicateNode> PredicateResolver for AstPredicateResolver<'_, Node> {
    fn resolve<'value>(
        &'value self,
        segments: &[PathSegment],
    ) -> Result<Option<ResolvedPredicateScalar<'value>>, PredicateTypeError> {
        let mut node = self.node;
        for segment in segments {
            let Some(child) = node.child(self.context, segment, self.haystack)? else {
                return Ok(None);
            };
            node = child;
        }
        node.scalar(self.haystack).map(Some)
    }
}

// JSON and TOML own their children; YAML's document-indexed lookup stays separate.
macro_rules! impl_ast_predicate_node {
    ($node_type:ty, $node:ident, $haystack:ident => $inner:expr, $span:expr;
        $value_type:ident, $mapping:ident, $boolean:ident $(, $null:ident)?;
        $key:ident, $entries:ident => $lookup:expr;
        $number:pat $(, written: $written:pat)?
    ) => {
        impl PredicateNode for $node_type {
            type Context = ();

            fn child<'node>(
                &'node $node,
                _context: &'node Self::Context,
                segment: &PathSegment,
                _haystack: &'node str,
            ) -> Result<Option<&'node Self>, PredicateTypeError> {
                match (segment, $inner) {
                    (PathSegment::Key($key), $value_type::$mapping($entries)) => Ok($lookup),
                    (PathSegment::Index(index), $value_type::Array(values)) => {
                        Ok(values.get(*index))
                    }
                    (PathSegment::Key(_), _) | (PathSegment::Index(_), _) => {
                        Err(PredicateTypeError::InvalidContainer)
                    }
                }
            }

            fn scalar<'node>(
                &'node $node,
                $haystack: &'node str,
            ) -> Result<ResolvedPredicateScalar<'node>, PredicateTypeError> {
                let (start, end) = $span;
                let value = match $inner {
                    $($value_type::$null => PredicateValue::Null,)?
                    $value_type::$boolean(value) => PredicateValue::Bool(*value),
                    $number => PredicateValue::Number(parse_numeric_text(&$haystack[start..end])?),
                    $value_type::String(value) => PredicateValue::String(value.as_ref()),
                    $($written => PredicateValue::String(&$haystack[start..end]),)?
                    $value_type::Array(_) | $value_type::$mapping(_) => {
                        return Err(PredicateTypeError::MissingScalar);
                    }
                };
                Ok(ResolvedPredicateScalar { start, end, value })
            }
        }
    };
}

impl_ast_predicate_node!(json_spanned::Value, self, haystack => self.get_ref(), self.span();
    JsonValueInner, Object, Bool, Null;
    key, entries => entries.get(key.as_str());
    JsonValueInner::Number(_)
);

impl PredicateNode for YamlNode {
    type Context = YamlDocument;

    fn child<'node>(
        &'node self,
        document: &'node Self::Context,
        segment: &PathSegment,
        haystack: &'node str,
    ) -> Result<Option<&'node Self>, PredicateTypeError> {
        match (segment, &self.data) {
            (PathSegment::Key(expected), YamlNodeData::Mapping { pairs, .. }) => {
                let mut merges = false;
                for pair in pairs {
                    let key = yaml_document_key_text(document, pair.key, haystack);
                    if key == "<<" && is_yaml_merge_key(document, pair.key) {
                        merges = true;
                    } else if key == expected
                        && let Some(child) = document.get_node(pair.value)
                    {
                        return Ok(Some(child));
                    }
                }
                // Merged keys never replace the mapping's own, so they're only looked up after.
                Ok(merges
                    .then(|| {
                        yaml_mapping_entries(document, pairs, haystack)
                            .find(|&(key, _)| key == expected)
                    })
                    .flatten()
                    .map(|(_, child)| child))
            }
            (PathSegment::Index(index), YamlNodeData::Sequence { items, .. }) => Ok(items
                .get(*index)
                .and_then(|child_id| document.get_node(*child_id))),
            (PathSegment::Key(_), _) | (PathSegment::Index(_), _) => {
                Err(PredicateTypeError::InvalidContainer)
            }
        }
    }

    fn scalar<'node>(
        &'node self,
        haystack: &'node str,
    ) -> Result<ResolvedPredicateScalar<'node>, PredicateTypeError> {
        let YamlNodeData::Scalar { value, style } = &self.data else {
            return Err(PredicateTypeError::MissingScalar);
        };
        let (start, end) =
            yaml_document_byte_range(self).ok_or(PredicateTypeError::MissingScalar)?;
        let node_text = &haystack[start..end];
        let value = match classify_yaml_plain_scalar(node_text, *style) {
            PlainYamlScalarKind::Null => PredicateValue::Null,
            PlainYamlScalarKind::Bool(value) => PredicateValue::Bool(value),
            PlainYamlScalarKind::Number => PredicateValue::Number(parse_numeric_text(node_text)?),
            PlainYamlScalarKind::String => PredicateValue::String(value.as_str()),
        };
        Ok(ResolvedPredicateScalar { start, end, value })
    }
}

// TOML datetimes compare as written, like RFC 3339 timestamps in JSON and YAML strings.
// Only scalars are compared, so a table's span, which takes a walk over it, isn't needed.
impl_ast_predicate_node!(TomlItem<'_>, self, haystack => self.value(),
    (self.span().start as usize, self.span().end as usize);
    TomlValue, Table, Boolean;
    key, entries => entries.get(key);
    TomlValue::Integer(_) | TomlValue::Float(_), written: TomlValue::DateTime(_)
);

pub(super) fn parse_numeric_text(raw_text: &str) -> Result<Number, PredicateTypeError> {
    if let Some(value) = parse_small_integer(raw_text.as_bytes()) {
        return Ok(Number::Integer(value));
    }
    let normalized = if raw_text.contains('_') {
        Cow::Owned(raw_text.replace('_', ""))
    } else {
        Cow::Borrowed(raw_text)
    };
    parse_radix_integer(&normalized)
        .or_else(|| Number::parse(&normalized))
        .or_else(|| yaml_special_float(&normalized).map(Number::Float))
        .or_else(|| yaml_base_60(&normalized))
        .ok_or(PredicateTypeError::InvalidNumber)
}

/// Parses YAML and TOML integers written with a `0x`, `0o`, or `0b` prefix.
fn parse_radix_integer(text: &str) -> Option<Number> {
    let (negative, unsigned) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let radix = match unsigned.get(..2)? {
        "0x" | "0X" => 16,
        "0o" | "0O" => 8,
        "0b" | "0B" => 2,
        _ => return None,
    };
    let digits = &unsigned[2..];
    if digits.starts_with(['+', '-']) {
        return None;
    }
    let magnitude = i128::from(u64::from_str_radix(digits, radix).ok()?);
    let value = if negative { -magnitude } else { magnitude };
    Some(i64::try_from(value).map_or(Number::Float(value as f64), Number::Integer))
}

/// Fast path for decimal integers of at most 18 digits, which always fit an `i64`.
fn parse_small_integer(text: &[u8]) -> Option<i64> {
    let (negative, digits) = match text.split_first() {
        Some((b'-', digits)) => (true, digits),
        _ => (false, text),
    };
    if digits.is_empty() || digits.len() > 18 {
        return None;
    }
    let magnitude = digits.iter().try_fold(0_i64, |value, &byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + i64::from(byte - b'0'))
    })?;
    Some(if negative { -magnitude } else { magnitude })
}

/// Parses the JSON scalar at `text[start..end]`, whose bounds sit on ASCII delimiters.
pub(super) fn parse_json_predicate_value(
    text: &str,
    start: usize,
    end: usize,
) -> Option<PredicateValue<'_>> {
    if start >= end || start >= text.len() {
        return None;
    }
    match text.as_bytes()[start] {
        b'"' => {
            let inner = text.get(start + 1..end.saturating_sub(1))?;
            if inner.contains('\\') {
                return None;
            }
            Some(PredicateValue::String(inner))
        }
        b'n' => Some(PredicateValue::Null),
        b't' => Some(PredicateValue::Bool(true)),
        b'f' => Some(PredicateValue::Bool(false)),
        _ => parse_numeric_text(text.get(start..end)?)
            .ok()
            .map(PredicateValue::Number),
    }
}

pub(super) fn parse_yaml_predicate_value(raw_text: &str) -> Option<PredicateValue<'_>> {
    let trimmed = raw_text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_wrapped_quote(trimmed) {
        return Some(PredicateValue::String(&trimmed[1..trimmed.len() - 1]));
    }
    match classify_yaml_plain_scalar(trimmed, YamlScalarStyle::Plain) {
        PlainYamlScalarKind::Null => Some(PredicateValue::Null),
        PlainYamlScalarKind::Bool(value) => Some(PredicateValue::Bool(value)),
        PlainYamlScalarKind::Number => parse_numeric_text(trimmed).ok().map(PredicateValue::Number),
        PlainYamlScalarKind::String => Some(PredicateValue::String(trimmed)),
    }
}

pub(super) fn parse_toml_predicate_value(raw_text: &str) -> Option<PredicateValue<'_>> {
    let trimmed = raw_text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_wrapped_quote(trimmed) {
        return Some(PredicateValue::String(&trimmed[1..trimmed.len() - 1]));
    }
    if trimmed.eq_ignore_ascii_case("true") {
        return Some(PredicateValue::Bool(true));
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return Some(PredicateValue::Bool(false));
    }
    Some(
        parse_numeric_text(trimmed)
            .ok()
            .map_or(PredicateValue::String(trimmed), PredicateValue::Number),
    )
}

fn is_wrapped_quote(text: &str) -> bool {
    text.len() >= 2
        && matches!(text.as_bytes()[0], b'"' | b'\'')
        && text.as_bytes()[0] == *text.as_bytes().last().unwrap_or(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_parse_exactly_and_other_numbers_as_floats() {
        for (text, expected) in [
            ("0", 0),
            ("-0", 0),
            ("007", 7),
            ("-42", -42),
            ("+12", 12),
            ("1_000", 1000),
            ("999999999999999999", 999_999_999_999_999_999),
            ("9007199254740993", 9_007_199_254_740_993),
            ("-9223372036854775808", i64::MIN),
        ] {
            assert!(
                matches!(parse_numeric_text(text), Ok(Number::Integer(value)) if value == expected),
                "{text}"
            );
        }
        for (text, expected) in [
            ("1.5", 1.5),
            ("1e3", 1e3),
            ("18446744073709551616", 18_446_744_073_709_551_616.0),
        ] {
            assert!(
                matches!(parse_numeric_text(text), Ok(Number::Float(value)) if value == expected),
                "{text}"
            );
        }
        for text in ["", "-", "+1", "1.5", "1_000", "0x1F", "1234567890123456789"] {
            assert_eq!(parse_small_integer(text.as_bytes()), None, "{text}");
        }
    }

    #[test]
    fn radix_and_base_60_integers_parse_with_signs_and_separators() {
        for (text, expected) in [
            ("0x1F", 31),
            ("0XdEaD_bEeF", 3_735_928_559),
            ("-0o17", -15),
            ("+0b1010", 10),
            ("-0x8000000000000000", i64::MIN),
        ] {
            assert!(
                matches!(parse_numeric_text(text), Ok(Number::Integer(value)) if value == expected),
                "{text}"
            );
        }
        for (text, expected) in [("1:30", 90), ("-1:30", -90), ("190:20:30", 685_230)] {
            assert!(
                matches!(parse_numeric_text(text), Ok(Number::Integer(value)) if value == expected),
                "{text}"
            );
        }
        assert_eq!(parse_numeric_text("1:30.5"), Ok(Number::Float(90.5)));
        assert_eq!(parse_numeric_text("1:30."), Ok(Number::Float(90.0)));
        assert_eq!(
            parse_numeric_text("0xFFFFFFFFFFFFFFFF"),
            Ok(Number::Float(u64::MAX as f64))
        );
        for text in ["0x", "0x+5", "0o8", "0b2", "0xg"] {
            assert_eq!(
                parse_numeric_text(text),
                Err(PredicateTypeError::InvalidNumber),
                "{text}"
            );
        }
    }
}
