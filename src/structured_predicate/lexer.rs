/// Yields byte offsets outside quoted text, excluding quotes and escaped bytes.
fn unquoted_indices(text: &str) -> impl Iterator<Item = usize> + '_ {
    let mut quote = None;
    let mut escaped = false;
    text.bytes().enumerate().filter_map(move |(index, byte)| {
        if escaped {
            escaped = false;
            return None;
        }
        if let Some(current) = quote {
            if byte == b'\\' {
                escaped = true;
            } else if byte == current {
                quote = None;
            }
            return None;
        }
        if matches!(byte, b'"' | b'\'') {
            quote = Some(byte);
            return None;
        }
        Some(index)
    })
}

/// Splits `text` at unquoted, space-delimited AND tokens, excluding BETWEEN's bound separator.
/// Returns trimmed clauses, including empty clauses for the parser to reject.
pub(super) fn split_clauses(text: &str) -> Vec<&str> {
    let mut clauses = Vec::new();
    let mut start = 0;
    let mut inside_between = false;
    for index in unquoted_indices(text) {
        if starts_with_keyword(text.as_bytes(), index, "BETWEEN") {
            inside_between = true;
        } else if starts_with_keyword(text.as_bytes(), index, "AND") {
            if inside_between {
                inside_between = false;
            } else {
                clauses.push(text[start..index].trim());
                start = index + "AND".len() + 1;
            }
        }
    }
    clauses.push(text[start..].trim());
    clauses
}

/// Splits trimmed text around the first unquoted, whitespace-delimited `keyword`, compared
/// ASCII case-insensitively.
pub(super) fn split_keyword_once<'a>(text: &'a str, keyword: &str) -> Option<(&'a str, &'a str)> {
    let index =
        unquoted_indices(text).find(|&index| keyword_at(text.as_bytes(), index, keyword))?;
    Some((text[..index].trim(), text[index + keyword.len()..].trim()))
}

/// Returns the nonempty operands and first supported unquoted operator in `text`.
/// Returns `None` if no operator has both operands, trying two-byte operators first.
pub(super) fn split_binary_clause(text: &str) -> Option<(&str, &str, &str)> {
    for index in unquoted_indices(text) {
        for operator in [">=", "<=", "!=", "==", ">", "<", "="] {
            if text.as_bytes()[index..].starts_with(operator.as_bytes()) {
                let left = text[..index].trim();
                let right = text[index + operator.len()..].trim();
                if !left.is_empty() && !right.is_empty() {
                    return Some((left, operator, right));
                }
            }
        }
    }
    None
}

fn starts_with_keyword(bytes: &[u8], index: usize, keyword: &str) -> bool {
    index > 0
        && bytes[index - 1] == b' '
        && bytes.get(index + keyword.len()) == Some(&b' ')
        && keyword_at(bytes, index, keyword)
}

fn keyword_at(bytes: &[u8], index: usize, keyword: &str) -> bool {
    let keyword_bytes = keyword.as_bytes();
    if bytes[index..].len() < keyword_bytes.len() {
        return false;
    }
    let before_ok = index == 0 || bytes[index - 1].is_ascii_whitespace();
    let after_index = index + keyword_bytes.len();
    let after_ok = after_index == bytes.len() || bytes[after_index].is_ascii_whitespace();
    before_ok && after_ok && bytes[index..after_index].eq_ignore_ascii_case(keyword_bytes)
}

#[cfg(test)]
mod tests {
    use super::{split_binary_clause, split_clauses, split_keyword_once, unquoted_indices};

    #[test]
    fn excludes_quoted_tokens_and_escaped_quotes() {
        let text = r#"$.name = "a\" AND > b" AND $.count >= 2"#;
        assert_eq!(
            split_clauses(text),
            vec![r#"$.name = "a\" AND > b""#, "$.count >= 2"]
        );
        assert_eq!(
            split_keyword_once(text, "AND"),
            Some((r#"$.name = "a\" AND > b""#, "$.count >= 2"))
        );
        assert_eq!(
            split_binary_clause("$['>'] >= 2"),
            Some(("$['>']", ">=", "2"))
        );
        assert_eq!(unquoted_indices("'a\\'b'c").collect::<Vec<_>>(), vec![6]);
    }

    #[test]
    fn preserves_between_and_keyword_boundaries() {
        let text = "$.a BETWEEN 1 AND 2 AND $.b < 3";
        assert_eq!(split_clauses(text), vec!["$.a BETWEEN 1 AND 2", "$.b < 3"]);
        assert_eq!(
            split_keyword_once("$.a\tbetween\t1", "BETWEEN"),
            Some(("$.a", "1"))
        );
        // Clause separators deliberately require spaces, unlike the single-keyword splitter.
        assert_eq!(
            split_clauses("$.a = 1\tAND\t$.b = 2"),
            vec!["$.a = 1\tAND\t$.b = 2"]
        );
        assert_eq!(split_keyword_once("$.a = 'AND'", "AND"), None);
        assert_eq!(split_binary_clause("$.a >"), None);
        assert_eq!(split_binary_clause("$.a >="), Some(("$.a", ">", "=")));
        assert_eq!(split_binary_clause("$.a == 2"), Some(("$.a", "==", "2")));
    }

    #[test]
    fn keeps_utf8_byte_offsets_and_unterminated_quotes() {
        assert_eq!(
            split_binary_clause("$.名 != 'é'"),
            Some(("$.名", "!=", "'é'"))
        );
        assert_eq!(split_keyword_once("'unterminated AND x", "AND"), None);
        assert_eq!(unquoted_indices("x'\\").collect::<Vec<_>>(), vec![0]);
    }
}
