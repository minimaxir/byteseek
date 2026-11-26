use std::{collections::HashMap, iter::Peekable, vec::IntoIter};

use super::{BooleanExpr, ParsedExpr};
use crate::SearchError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Token {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Literal(String),
}

/// Deepest nesting of parentheses and negations accepted, bounding recursion depth.
const MAX_NESTING: usize = 128;
/// Most `AND` and `OR` operators accepted in one query, since a chain of them recurses like
/// nesting.
const MAX_OPERATORS: usize = 1024;

pub(super) struct Parser {
    tokens: Peekable<IntoIter<Token>>,
    pattern_index: usize,
    nesting: usize,
    operators: usize,
}

impl Parser {
    pub(super) fn new(tokens: Vec<Token>, pattern_index: usize) -> Self {
        Self {
            tokens: tokens.into_iter().peekable(),
            pattern_index,
            nesting: 0,
            operators: 0,
        }
    }

    pub(super) fn parse_expression(&mut self) -> Result<ParsedExpr, SearchError> {
        let expression = self.parse_binary(1)?;
        if self.tokens.peek().is_some() {
            return Err(self.invalid("unexpected trailing tokens in boolean query"));
        }
        Ok(expression)
    }

    /// Raising the right operand's minimum precedence preserves left associativity.
    fn parse_binary(&mut self, minimum_precedence: u8) -> Result<ParsedExpr, SearchError> {
        let mut expr = self.parse_primary()?;
        loop {
            let precedence = match self.tokens.peek() {
                Some(Token::Or) => 1,
                Some(Token::And) => 2,
                Some(Token::LParen | Token::RParen | Token::Not | Token::Literal(_)) | None => {
                    break;
                }
            };
            if precedence < minimum_precedence {
                break;
            }
            if self.operators == MAX_OPERATORS {
                return Err(self.invalid("boolean query has too many operators"));
            }
            self.operators += 1;
            let is_and = matches!(self.tokens.next(), Some(Token::And));
            let left = Box::new(expr);
            let right = Box::new(self.parse_binary(precedence + 1)?);
            expr = if is_and {
                ParsedExpr::And(left, right)
            } else {
                ParsedExpr::Or(left, right)
            };
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<ParsedExpr, SearchError> {
        match self.tokens.next() {
            Some(Token::Literal(text)) => Ok(ParsedExpr::Literal(text)),
            Some(Token::Not) => {
                let operand = self.nested(Self::parse_primary)?;
                Ok(ParsedExpr::Not(Box::new(operand)))
            }
            Some(Token::LParen) => {
                let expr = self.nested(|parser| parser.parse_binary(1))?;
                match self.tokens.next() {
                    Some(Token::RParen) => Ok(expr),
                    Some(
                        Token::LParen | Token::And | Token::Or | Token::Not | Token::Literal(_),
                    )
                    | None => Err(self.invalid("boolean query is missing a closing `)`")),
                }
            }
            Some(Token::RParen) => Err(self.invalid("boolean query contains an unexpected `)`")),
            Some(Token::And) | Some(Token::Or) | None => {
                Err(self.invalid("boolean query is missing a literal or parenthesized clause"))
            }
        }
    }

    /// Parses one level deeper with `parse`, refusing levels past [`MAX_NESTING`].
    fn nested(
        &mut self,
        parse: impl FnOnce(&mut Self) -> Result<ParsedExpr, SearchError>,
    ) -> Result<ParsedExpr, SearchError> {
        if self.nesting == MAX_NESTING {
            return Err(self.invalid("boolean query is nested too deeply"));
        }
        self.nesting += 1;
        let parsed = parse(self);
        self.nesting -= 1;
        parsed
    }

    fn invalid(&self, message: &str) -> SearchError {
        invalid_boolean_pattern(self.pattern_index, message)
    }
}

pub(super) fn intern_expression_terms(
    expr: ParsedExpr,
    unique_terms: &mut Vec<Vec<u8>>,
    term_indexes: &mut HashMap<Vec<u8>, usize>,
    pattern_index: usize,
) -> Result<BooleanExpr, SearchError> {
    let invalid = |message| invalid_boolean_pattern(pattern_index, message);
    expr.try_map(&mut |text| {
        if text.is_empty() {
            return Err(invalid("boolean query literal cannot be empty"));
        }
        if text.contains(['\n', '\r']) {
            return Err(invalid(
                "boolean query literals cannot contain newline characters",
            ));
        }

        let key = text.into_bytes();
        if let Some(&index) = term_indexes.get(key.as_slice()) {
            return Ok(index);
        }

        let index = unique_terms.len();
        unique_terms.push(key.clone());
        term_indexes.insert(key, index);
        Ok(index)
    })
}

pub(super) fn tokenize(text: &str, pattern_index: usize) -> Result<Vec<Token>, SearchError> {
    let invalid = |message| invalid_boolean_pattern(pattern_index, message);
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut pos = 0_usize;

    while let Some(&byte) = bytes.get(pos) {
        if byte.is_ascii_whitespace() {
            pos += 1;
            continue;
        }
        match byte {
            b'(' | b')' => {
                tokens.push(if byte == b'(' {
                    Token::LParen
                } else {
                    Token::RParen
                });
                pos += 1;
            }
            b'"' | b'\'' => {
                let quote = bytes[pos];
                pos += 1;
                let mut literal = Vec::new();

                while pos < bytes.len() {
                    let mut byte = bytes[pos];
                    pos += 1;
                    if byte == b'\\' {
                        byte = *bytes.get(pos).ok_or_else(|| {
                            invalid("boolean query string literal ends with a trailing escape")
                        })?;
                        pos += 1;
                    } else if byte == quote {
                        break;
                    }
                    literal.push(byte);
                }

                if pos > bytes.len() || bytes.get(pos.saturating_sub(1)) != Some(&quote) {
                    return Err(invalid("unterminated quoted boolean literal"));
                }

                // Only single-byte backslashes were removed, so the text is still UTF-8.
                let literal = String::from_utf8(literal)
                    .expect("dropping ASCII bytes keeps text valid UTF-8");
                tokens.push(Token::Literal(literal));
            }
            _ => {
                let start = pos;
                while pos < bytes.len()
                    && !bytes[pos].is_ascii_whitespace()
                    && bytes[pos] != b'('
                    && bytes[pos] != b')'
                {
                    pos += 1;
                }
                let token_text = &text[start..pos];
                if token_text.eq_ignore_ascii_case("AND") {
                    tokens.push(Token::And);
                } else if token_text.eq_ignore_ascii_case("OR") {
                    tokens.push(Token::Or);
                } else if token_text.eq_ignore_ascii_case("NOT") {
                    tokens.push(Token::Not);
                } else {
                    tokens.push(Token::Literal(token_text.to_owned()));
                }
            }
        }
    }

    if tokens.is_empty() {
        return Err(invalid("boolean query must contain at least one literal"));
    }

    Ok(tokens)
}

fn invalid_boolean_pattern(index: usize, message: impl Into<String>) -> SearchError {
    SearchError::InvalidBooleanPattern {
        index,
        message: message.into(),
    }
}
