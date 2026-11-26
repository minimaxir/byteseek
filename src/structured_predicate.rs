use std::cmp::Ordering;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::SearchError;
use crate::json_scanner::PathSegment;

use self::lexer::{split_binary_clause, split_clauses, split_keyword_once};

mod lexer;

#[derive(Debug, Clone)]
pub(crate) struct PredicatePattern {
    pub(crate) clauses: Vec<PredicateClause>,
    pub(crate) pattern_index: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PredicateClause {
    pub(crate) segments: Vec<PathSegment>,
    pub(crate) operator: PredicateOperator,
}

#[derive(Debug, Clone)]
pub(crate) enum PredicateOperator {
    Eq(PredicateLiteral),
    NotEq(PredicateLiteral),
    GreaterThan(PredicateLiteral),
    GreaterThanOrEqual(PredicateLiteral),
    LessThan(PredicateLiteral),
    LessThanOrEqual(PredicateLiteral),
    Between {
        lower: PredicateLiteral,
        upper: PredicateLiteral,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum PredicateLiteral {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Timestamp(OffsetDateTime),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PredicateValue<'a> {
    Null,
    Bool(bool),
    Number(Number),
    String(&'a str),
}

/// A number in a predicate or a document. Integers stay exact, so IDs above 2^53 compare
/// correctly; a comparison involving a float compares as `f64`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Number {
    Integer(i64),
    Float(f64),
}

impl Number {
    /// Parses a decimal integer or float.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        text.parse()
            .map(Self::Integer)
            .or_else(|_| text.parse().map(Self::Float))
            .ok()
    }

    fn as_f64(self) -> f64 {
        match self {
            Self::Integer(value) => value as f64,
            Self::Float(value) => value,
        }
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        self.partial_cmp(other) == Some(Ordering::Equal)
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (*self, *other) {
            (Self::Integer(left), Self::Integer(right)) => Some(left.cmp(&right)),
            (left, right) => left.as_f64().partial_cmp(&right.as_f64()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PredicateTypeError {
    #[error("path resolves to a non-scalar value")]
    MissingScalar,
    #[error("path resolves to null")]
    NullValue,
    #[error("path traverses through the wrong container type")]
    InvalidContainer,
    #[error("path does not contain a boolean value")]
    InvalidBoolean,
    #[error("path does not contain a numeric value")]
    InvalidNumber,
    #[error("path does not contain an RFC 3339 timestamp")]
    InvalidTimestamp,
    #[error("path value type does not match the predicate literal")]
    TypeMismatch,
}

pub(crate) fn is_predicate_pattern(text: &str) -> bool {
    if !text.starts_with('$') {
        return false;
    }

    contains_keyword_outside_quotes(text, "AND")
        || contains_keyword_outside_quotes(text, "BETWEEN")
        || contains_operator_outside_quotes(text)
}

pub(crate) fn parse_predicate_pattern(
    text: &str,
    pattern_index: usize,
) -> Result<PredicatePattern, SearchError> {
    let mut clauses = Vec::new();
    for clause_text in split_clauses(text) {
        clauses.push(parse_clause(clause_text, pattern_index)?);
    }

    if clauses.is_empty() {
        return Err(pattern_error(
            pattern_index,
            "predicate must contain at least one clause",
        ));
    }

    Ok(PredicatePattern {
        clauses,
        pattern_index,
    })
}

pub(crate) fn evaluate_clause(
    value: PredicateValue<'_>,
    operator: &PredicateOperator,
) -> Result<bool, PredicateTypeError> {
    match operator {
        PredicateOperator::Eq(literal) => compare_equality(value, literal, false),
        PredicateOperator::NotEq(literal) => compare_equality(value, literal, true),
        PredicateOperator::GreaterThan(literal) => {
            compare_ordering(value, literal, Ordering::is_gt)
        }
        PredicateOperator::GreaterThanOrEqual(literal) => {
            compare_ordering(value, literal, Ordering::is_ge)
        }
        PredicateOperator::LessThan(literal) => compare_ordering(value, literal, Ordering::is_lt),
        PredicateOperator::LessThanOrEqual(literal) => {
            compare_ordering(value, literal, Ordering::is_le)
        }
        PredicateOperator::Between { lower, upper } => compare_between(value, lower, upper),
    }
}

fn parse_clause(text: &str, pattern_index: usize) -> Result<PredicateClause, SearchError> {
    if let Some((path_text, rest)) = split_keyword_once(text, "BETWEEN") {
        let (lower_text, upper_text) = split_keyword_once(rest, "AND").ok_or_else(|| {
            pattern_error(
                pattern_index,
                "between predicate must contain an upper bound after `and`",
            )
        })?;
        let segments = parse_predicate_path(path_text.trim(), pattern_index)?;
        let lower = parse_literal(lower_text.trim(), pattern_index)?;
        let upper = parse_literal(upper_text.trim(), pattern_index)?;
        validate_between_bounds(&lower, &upper, pattern_index)?;
        return Ok(PredicateClause {
            segments,
            operator: PredicateOperator::Between { lower, upper },
        });
    }

    let (path_text, operator_text, literal_text) = split_binary_clause(text).ok_or_else(|| {
        pattern_error(
            pattern_index,
            "predicate clause is missing a supported operator",
        )
    })?;
    let segments = parse_predicate_path(path_text.trim(), pattern_index)?;
    let literal = parse_literal(literal_text.trim(), pattern_index)?;
    if matches!(operator_text, ">" | ">=" | "<" | "<=")
        && !matches!(
            literal,
            PredicateLiteral::Number(_) | PredicateLiteral::Timestamp(_)
        )
    {
        return Err(pattern_error(
            pattern_index,
            "ordering predicates require numeric or RFC 3339 timestamp literals",
        ));
    }
    let operator = match operator_text {
        "=" | "==" => PredicateOperator::Eq(literal),
        "!=" => PredicateOperator::NotEq(literal),
        ">" => PredicateOperator::GreaterThan(literal),
        ">=" => PredicateOperator::GreaterThanOrEqual(literal),
        "<" => PredicateOperator::LessThan(literal),
        "<=" => PredicateOperator::LessThanOrEqual(literal),
        _ => {
            return Err(pattern_error(
                pattern_index,
                format!("unsupported predicate operator `{operator_text}`"),
            ));
        }
    };

    Ok(PredicateClause { segments, operator })
}

fn validate_between_bounds(
    lower: &PredicateLiteral,
    upper: &PredicateLiteral,
    pattern_index: usize,
) -> Result<(), SearchError> {
    match (lower, upper) {
        (PredicateLiteral::Number(_), PredicateLiteral::Number(_))
        | (PredicateLiteral::Timestamp(_), PredicateLiteral::Timestamp(_)) => Ok(()),
        _ => Err(pattern_error(
            pattern_index,
            "between predicates require matching numeric or RFC 3339 timestamp bounds",
        )),
    }
}

fn pattern_error(pattern_index: usize, message: impl Into<String>) -> SearchError {
    SearchError::InvalidStructuredPattern {
        index: pattern_index,
        message: message.into(),
    }
}

fn parse_predicate_path(
    path_text: &str,
    pattern_index: usize,
) -> Result<Vec<PathSegment>, SearchError> {
    if !path_text.starts_with('$') {
        return Err(pattern_error(
            pattern_index,
            "predicate path must start with `$`",
        ));
    }
    let invalid_path = || {
        pattern_error(
            pattern_index,
            format!("invalid predicate path `{path_text}`"),
        )
    };
    let mut segments = Vec::new();
    let mut pos = 1_usize;
    let path_bytes = path_text.as_bytes();

    while pos < path_bytes.len() {
        match path_bytes[pos] {
            b'.' => {
                pos += 1;
                let start = pos;
                while pos < path_bytes.len() && path_bytes[pos] != b'.' && path_bytes[pos] != b'[' {
                    pos += 1;
                }
                if pos == start {
                    return Err(invalid_path());
                }
                segments.push(PathSegment::Key(path_text[start..pos].to_owned()));
            }
            b'[' => {
                pos += 1;
                if pos < path_bytes.len() && matches!(path_bytes[pos], b'"' | b'\'') {
                    let quote = path_bytes[pos];
                    pos += 1;
                    let start = pos;
                    while pos < path_bytes.len() {
                        if path_bytes[pos] == b'\\' {
                            pos += 2;
                            continue;
                        }
                        if path_bytes[pos] == quote {
                            break;
                        }
                        pos += 1;
                    }
                    if pos >= path_bytes.len() {
                        return Err(pattern_error(
                            pattern_index,
                            format!("unterminated quoted predicate path `{path_text}`"),
                        ));
                    }
                    let key = parse_quoted_text(&path_text[start - 1..=pos], pattern_index)?;
                    segments.push(PathSegment::Key(key));
                    pos += 1;
                } else {
                    let start = pos;
                    while pos < path_bytes.len() && path_bytes[pos] != b']' {
                        pos += 1;
                    }
                    let index = path_text[start..pos].parse().map_err(|_| {
                        pattern_error(
                            pattern_index,
                            format!("invalid array index in predicate path `{path_text}`"),
                        )
                    })?;
                    segments.push(PathSegment::Index(index));
                }
                if pos >= path_bytes.len() || path_bytes[pos] != b']' {
                    return Err(invalid_path());
                }
                pos += 1;
            }
            _ => return Err(invalid_path()),
        }
    }

    Ok(segments)
}

fn parse_literal(text: &str, pattern_index: usize) -> Result<PredicateLiteral, SearchError> {
    if text.is_empty() {
        return Err(pattern_error(
            pattern_index,
            "predicate literal cannot be empty",
        ));
    }

    if let Some(unquoted) = parse_optional_quoted_text(text, pattern_index)? {
        // Quotes make a string, as in `== "2018"`, or a timestamp, whose text needs no quotes.
        return Ok(OffsetDateTime::parse(&unquoted, &Rfc3339).map_or(
            PredicateLiteral::String(unquoted),
            PredicateLiteral::Timestamp,
        ));
    }

    Ok(classify_literal(text))
}

fn classify_literal(text: &str) -> PredicateLiteral {
    let normalized = text.trim();
    if normalized.eq_ignore_ascii_case("null") {
        return PredicateLiteral::Null;
    }
    if normalized.eq_ignore_ascii_case("true") {
        return PredicateLiteral::Bool(true);
    }
    if normalized.eq_ignore_ascii_case("false") {
        return PredicateLiteral::Bool(false);
    }
    if let Some(number) = Number::parse(normalized) {
        return PredicateLiteral::Number(number);
    }
    if let Ok(timestamp) = OffsetDateTime::parse(normalized, &Rfc3339) {
        return PredicateLiteral::Timestamp(timestamp);
    }
    PredicateLiteral::String(normalized.to_owned())
}

/// Parses an RFC 3339 timestamp. The common `YYYY-MM-DDTHH:MM:SSZ` form is decoded directly;
/// other spellings go through the general parser.
fn parse_timestamp(text: &str) -> Result<OffsetDateTime, PredicateTypeError> {
    canonical_utc_timestamp(text.as_bytes())
        .or_else(|| OffsetDateTime::parse(text, &Rfc3339).ok())
        .ok_or(PredicateTypeError::InvalidTimestamp)
}

fn canonical_utc_timestamp(bytes: &[u8]) -> Option<OffsetDateTime> {
    let bytes: &[u8; 20] = bytes.try_into().ok()?;
    let separators_match = bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z';
    if !separators_match {
        return None;
    }
    let number = |range: std::ops::Range<usize>| {
        bytes[range].iter().try_fold(0_u16, |value, &byte| {
            byte.is_ascii_digit()
                .then(|| value * 10 + u16::from(byte - b'0'))
        })
    };
    let month = time::Month::try_from(u8::try_from(number(5..7)?).ok()?).ok()?;
    let date = time::Date::from_calendar_date(
        i32::from(number(0..4)?),
        month,
        u8::try_from(number(8..10)?).ok()?,
    )
    .ok()?;
    // `from_hms` rejects second 60, leaving leap seconds to the general parser.
    let clock = time::Time::from_hms(
        u8::try_from(number(11..13)?).ok()?,
        u8::try_from(number(14..16)?).ok()?,
        u8::try_from(number(17..19)?).ok()?,
    )
    .ok()?;
    Some(time::PrimitiveDateTime::new(date, clock).assume_utc())
}

fn compare_equality(
    value: PredicateValue<'_>,
    literal: &PredicateLiteral,
    invert: bool,
) -> Result<bool, PredicateTypeError> {
    let matches = match (value, literal) {
        (PredicateValue::Null, PredicateLiteral::Null) => true,
        (PredicateValue::Null, _) => return Err(PredicateTypeError::NullValue),
        (PredicateValue::Bool(actual), PredicateLiteral::Bool(expected)) => actual == *expected,
        (PredicateValue::Number(actual), PredicateLiteral::Number(expected)) => actual == *expected,
        (PredicateValue::String(actual), PredicateLiteral::String(expected)) => actual == expected,
        (PredicateValue::String(actual), PredicateLiteral::Timestamp(expected)) => {
            let parsed = parse_timestamp(actual)?;
            parsed == *expected
        }
        (PredicateValue::Bool(_) | PredicateValue::Number(_) | PredicateValue::String(_), _) => {
            return Err(PredicateTypeError::TypeMismatch);
        }
    };

    Ok(if invert { !matches } else { matches })
}

fn compare_ordering(
    value: PredicateValue<'_>,
    literal: &PredicateLiteral,
    compare: impl FnOnce(Ordering) -> bool,
) -> Result<bool, PredicateTypeError> {
    let ordering = match (value, literal) {
        (PredicateValue::Number(actual), PredicateLiteral::Number(expected)) => {
            actual.partial_cmp(expected)
        }
        (PredicateValue::String(actual), PredicateLiteral::Timestamp(expected)) => {
            Some(parse_timestamp(actual)?.cmp(expected))
        }
        (PredicateValue::Null, _) => return Err(PredicateTypeError::NullValue),
        (PredicateValue::Bool(_), _) => return Err(PredicateTypeError::InvalidBoolean),
        (PredicateValue::String(_), PredicateLiteral::Number(_)) => {
            return Err(PredicateTypeError::InvalidNumber);
        }
        (PredicateValue::Number(_) | PredicateValue::String(_), _) => {
            return Err(PredicateTypeError::TypeMismatch);
        }
    };

    // NaN is unordered, so no ordering holds for it.
    Ok(ordering.is_some_and(compare))
}

fn compare_between(
    value: PredicateValue<'_>,
    lower: &PredicateLiteral,
    upper: &PredicateLiteral,
) -> Result<bool, PredicateTypeError> {
    match (value, lower, upper) {
        (
            PredicateValue::Number(actual),
            PredicateLiteral::Number(lower),
            PredicateLiteral::Number(upper),
        ) => Ok(*lower <= actual && actual <= *upper),
        (
            PredicateValue::String(actual),
            PredicateLiteral::Timestamp(lower),
            PredicateLiteral::Timestamp(upper),
        ) => {
            let parsed = parse_timestamp(actual)?;
            Ok(*lower <= parsed && parsed <= *upper)
        }
        (PredicateValue::Null, _, _) => Err(PredicateTypeError::NullValue),
        (PredicateValue::Bool(_), _, _) => Err(PredicateTypeError::InvalidBoolean),
        (PredicateValue::String(_), PredicateLiteral::Number(_), PredicateLiteral::Number(_)) => {
            Err(PredicateTypeError::InvalidNumber)
        }
        _ => Err(PredicateTypeError::TypeMismatch),
    }
}

fn contains_keyword_outside_quotes(text: &str, keyword: &str) -> bool {
    split_keyword_once(text, keyword).is_some()
}

/// Whether `text` is a comparison. A lone `=` with no space around it is the exact
/// `$.path=value` form instead, whose value is compared as written.
fn contains_operator_outside_quotes(text: &str) -> bool {
    split_binary_clause(text).is_some_and(|(path, operator, literal)| match operator {
        "=" => text.trim().len() > path.len() + operator.len() + literal.len(),
        _ => true,
    })
}

fn parse_optional_quoted_text(
    text: &str,
    pattern_index: usize,
) -> Result<Option<String>, SearchError> {
    if text.len() < 2 {
        return Ok(None);
    }
    let bytes = text.as_bytes();
    if !matches!(bytes[0], b'"' | b'\'') || bytes[0] != *bytes.last().unwrap_or(&0) {
        return Ok(None);
    }
    Ok(Some(parse_quoted_text(text, pattern_index)?))
}

fn parse_quoted_text(text: &str, pattern_index: usize) -> Result<String, SearchError> {
    let quote = text.as_bytes()[0];
    let mut output = String::with_capacity(text.len().saturating_sub(2));
    let mut chars = text[1..text.len() - 1].chars();

    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }

        let escaped = chars.next().ok_or_else(|| {
            pattern_error(
                pattern_index,
                "predicate string literal ends with a trailing escape",
            )
        })?;
        match escaped {
            '\\' => output.push('\\'),
            '"' if quote == b'"' => output.push('"'),
            '\'' if quote == b'\'' => output.push('\''),
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            other => output.push(other),
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaced_and_double_equals_are_predicates_while_bare_equals_is_a_path_value() {
        for predicate in [
            "$.a == true",
            "$.a==true",
            "$.a = \"x\"",
            "$.a =null",
            "$.a= 1",
            "$.a != 1",
            "$.a > 1",
        ] {
            assert!(is_predicate_pattern(predicate), "{predicate}");
        }
        for path_value in [
            "$.a=true",
            "$.a=\"x\"",
            "$.name=Zo\u{eb} \"Z\"",
            "$.a",
            "a = 1",
        ] {
            assert!(!is_predicate_pattern(path_value), "{path_value}");
        }
    }

    #[test]
    fn quoted_literals_are_strings_or_timestamps() {
        for (literal, expected) in [
            (r#""2018""#, "2018"),
            (r#"'true'"#, "true"),
            (r#""null""#, "null"),
            (r#"" padded ""#, " padded "),
        ] {
            assert!(
                matches!(parse_literal(literal, 0), Ok(PredicateLiteral::String(text)) if text == expected),
                "{literal}"
            );
        }
        assert!(matches!(
            parse_literal(r#""2026-03-18T10:59:00Z""#, 0),
            Ok(PredicateLiteral::Timestamp(_))
        ));
        assert!(matches!(
            parse_literal("2018", 0),
            Ok(PredicateLiteral::Number(Number::Integer(2018)))
        ));
        assert!(matches!(
            parse_literal("true", 0),
            Ok(PredicateLiteral::Bool(true))
        ));
    }

    #[test]
    fn canonical_timestamps_match_the_general_parser() {
        for text in [
            "2026-03-18T10:59:00Z",
            "2024-02-29T23:59:59Z",
            "0000-01-01T00:00:00Z",
            "9999-12-31T23:59:59Z",
        ] {
            let expected = OffsetDateTime::parse(text, &Rfc3339).expect("valid RFC 3339");
            assert_eq!(canonical_utc_timestamp(text.as_bytes()), Some(expected));
            assert_eq!(parse_timestamp(text), Ok(expected));
        }
    }

    #[test]
    fn non_canonical_timestamps_defer_to_the_general_parser() {
        for text in [
            "2026-03-18T10:59:00+02:00",
            "2026-03-18T10:59:00.5Z",
            "2026-03-18t10:59:00z",
            "2016-12-31T23:59:60Z",
            "2026-03-18 10:59:00Z",
        ] {
            assert_eq!(canonical_utc_timestamp(text.as_bytes()), None);
            assert_eq!(
                parse_timestamp(text).ok(),
                OffsetDateTime::parse(text, &Rfc3339).ok()
            );
        }
        for invalid in [
            "2023-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-03-18T24:00:00Z",
            "2026-03-18T10:60:00Z",
            "2026-0a-18T10:59:00Z",
        ] {
            assert_eq!(canonical_utc_timestamp(invalid.as_bytes()), None);
            assert_eq!(
                parse_timestamp(invalid),
                Err(PredicateTypeError::InvalidTimestamp)
            );
        }
    }
}
