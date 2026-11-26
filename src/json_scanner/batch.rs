use std::collections::{BTreeMap, HashMap};

use super::{
    ParsedPathQuery, PathSegment, decoded_json_key, find_key_in_object, next_item, object_member,
    skip_value, skip_ws,
};

#[derive(Debug, Default)]
struct PathNode {
    keys: HashMap<Vec<u8>, usize>,
    indexes: BTreeMap<usize, usize>,
    outputs: Vec<usize>,
}

/// A trie of JSON path queries, so shared prefixes are navigated once per document.
#[derive(Debug)]
pub(crate) struct JsonPathBatch {
    nodes: Vec<PathNode>,
    query_count: usize,
}

impl JsonPathBatch {
    pub(crate) fn new(queries: &[ParsedPathQuery]) -> Self {
        let mut nodes = vec![PathNode::default()];
        for (query_index, query) in queries.iter().enumerate() {
            let mut node_index = 0;
            for segment in &query.segments {
                let next_index = nodes.len();
                let child = match segment {
                    PathSegment::Key(key) => *nodes[node_index]
                        .keys
                        .entry(key.as_bytes().to_vec())
                        .or_insert(next_index),
                    PathSegment::Index(index) => *nodes[node_index]
                        .indexes
                        .entry(*index)
                        .or_insert(next_index),
                };
                if child == next_index {
                    nodes.push(PathNode::default());
                }
                node_index = child;
            }
            nodes[node_index].outputs.push(query_index);
        }
        Self {
            nodes,
            query_count: queries.len(),
        }
    }

    /// Returns the byte span of each query's value in `bytes`, in query order.
    pub(crate) fn navigate(&self, bytes: &[u8]) -> Vec<Option<(usize, usize)>> {
        let mut spans = vec![None; self.query_count];
        let mut pending = vec![(0, skip_ws(bytes, 0))];
        let mut visited = vec![false; self.nodes.len()];
        while let Some((node_index, start)) = pending.pop() {
            let node = &self.nodes[node_index];
            if !node.outputs.is_empty() {
                let end = skip_value(bytes, start);
                for &output in &node.outputs {
                    spans[output] = Some((start, end));
                }
            }
            match bytes.get(start) {
                Some(b'{') if !node.keys.is_empty() => {
                    self.visit_object(bytes, start, node, (&mut pending, &mut visited));
                }
                Some(b'[') if !node.indexes.is_empty() => {
                    let mut position = skip_ws(bytes, start + 1);
                    let mut current_index = 0;
                    for (&target_index, &child) in &node.indexes {
                        while current_index < target_index && position < bytes.len() {
                            let Some(next) = next_item(bytes, skip_value(bytes, position)) else {
                                position = bytes.len();
                                break;
                            };
                            position = next;
                            current_index += 1;
                        }
                        if position >= bytes.len() || bytes[position] == b']' {
                            break;
                        }
                        pending.push((child, position));
                    }
                }
                _ => {}
            }
        }
        spans
    }

    #[inline]
    fn visit_object(
        &self,
        bytes: &[u8],
        start: usize,
        node: &PathNode,
        (pending, visited): (&mut Vec<(usize, usize)>, &mut [bool]),
    ) {
        // With one key, stop at its first occurrence without scanning the rest.
        if node.keys.len() == 1 {
            let (key, &child) = node.keys.iter().next().expect("one key is present");

            if let Some(position) = find_key_in_object(bytes, start, key) {
                pending.push((child, position));
            }
            return;
        }

        let mut remaining = node.keys.len();
        let mut position = skip_ws(bytes, start + 1);
        while let Some((key_start, key_end, value)) = object_member(bytes, position) {
            position = value;
            let raw = &bytes[key_start..key_end];
            let child = match decoded_json_key(raw) {
                None => node.keys.get(raw),
                Some(decoded) => node.keys.get(decoded.as_bytes()),
            };
            if let Some(&child) = child
                && !visited[child]
            {
                // Duplicate keys resolve to the first occurrence, even if the descendant
                // is missing there.
                visited[child] = true;
                pending.push((child, position));
                remaining -= 1;
                if remaining == 0 {
                    break;
                }
            }
            let Some(next) = next_item(bytes, skip_value(bytes, position)) else {
                break;
            };
            position = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::JsonPathBatch;
    use crate::json_scanner::{json_navigate_path, parse_path_pattern};

    #[test]
    fn batch_matches_independent_navigation() {
        let patterns = [
            "$.events[2].value",
            "$.events[0].value",
            "$.events[1]",
            "$.events",
            "$.events[99]",
            "$.events[2].value",
            "$.a.missing",
            "$.b",
            "$",
            "$.events[1].value",
            "$.a",
            "$.escaped\\\"key",
            "$.empty[0]",
        ];
        let queries = patterns
            .iter()
            .enumerate()
            .map(|(index, pattern)| {
                parse_path_pattern(pattern.as_bytes(), index).expect("valid test path")
            })
            .collect::<Vec<_>>();
        let batch = JsonPathBatch::new(&queries);
        for document in [
            concat!(
                r#"{"events":[{"value":0},{"value":1},{"value":"last"}],"a":{},"#,
                r#""a":{"missing":1},"b":true,"escaped\"key":null,"empty":[]}"#,
            ),
            r#"{"events":[],"a":1,"b":false}"#,
            r#"{"events":[null,[],{}],"b":[1,2,3],"a":{"missing":2}}"#,
            r#"[]"#,
            r#"{}"#,
        ] {
            assert_eq!(
                batch.navigate(document.as_bytes()),
                queries
                    .iter()
                    .map(|query| { json_navigate_path(document.as_bytes(), &query.segments) })
                    .collect::<Vec<_>>(),
                "{document}"
            );
        }
    }

    #[test]
    fn sparse_array_queries_and_empty_batches() {
        let document = format!(
            "[{}]",
            (0..100)
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let queries = ["$[99]", "$[3]", "$[42]", "$[0]", "$[100]"]
            .iter()
            .enumerate()
            .map(|(index, pattern)| {
                parse_path_pattern(pattern.as_bytes(), index).expect("valid test path")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            JsonPathBatch::new(&queries).navigate(document.as_bytes()),
            queries
                .iter()
                .map(|query| { json_navigate_path(document.as_bytes(), &query.segments) })
                .collect::<Vec<_>>()
        );
        assert!(JsonPathBatch::new(&[]).navigate(b"{}").is_empty());
    }
}
