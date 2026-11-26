use std::borrow::Cow;

use json_spanned_value::spanned as json_spanned;
use libyaml_safer::{
    Document as YamlDocument, Node as YamlNode, NodeData as YamlNodeData, Parser as YamlParser,
};
use smallvec::SmallVec;
use toml_spanner::{Item as TomlItem, Value as TomlValue};

use super::fast::make_structured_match;
use super::path::format_path_with_base;
use super::predicate::{AstPredicateResolver, PredicateResolver, evaluate_compiled_predicate};
use super::value::{
    json_scalar_text, load_yaml_document, toml_item_span, toml_scalar_text,
    yaml_document_byte_range, yaml_document_scalar_text, yaml_mapping_entries,
};
use super::{PatternMatcher, StructuredEngine, with_stack_room};
use crate::jq_path::{append_array_segment, append_key_segment};
use crate::json_scanner::PathSegment;
use crate::occurrences::Needle;
use crate::structured_predicate::PredicateTypeError;
use crate::{
    SearchConfig, SearchError, StructuredMatch, StructuredMatchKind, StructuredPredicateHandling,
};

// Generates the same traversal for each parser's AST without type erasure.
macro_rules! visit_ast {
    ($method:ident($collector:ident, [$($context:ident: $context_type:ty)?],
        $node:ident: $node_type:ty) {
        span: $span:expr, full_span: $full_span:expr, scalar: $scalar:path,
        context: $resolver_context:expr,
        value: $value:expr,
        sequence: $sequence:pat => $children:expr,
        mapping: $mapping:pat => $entries:expr,
        key: $key:ident => $key_segment:expr,
        leaf: $leaf:pat $(,)?
    }) => {
        pub(super) fn $method(&mut $collector, $($context: $context_type,)? $node: $node_type)
            -> Result<(), SearchError>
        {
            if $collector.is_full() {
                return Ok(());
            }
            $collector.charge(NODE_WORK + $collector.path.len())?;
            if $collector.record_nodes && let Some((start, end)) = $span {
                let text = &$collector.haystack[start..end];
                $collector.record_node((start, end), $full_span, $scalar($node, text));
                if $collector.is_full() {
                    return Ok(());
                }
            }
            match $value {
                $sequence => with_stack_room(|| {
                    for (index, child) in $children {
                        $collector.with_array_index(index, |collector| {
                            collector.$method($($context,)? child)
                        })?;
                        if $collector.is_full() {
                            break;
                        }
                    }
                    Ok::<_, SearchError>(())
                })?,
                $mapping => {
                    let resolver = AstPredicateResolver::new(
                        $resolver_context, $node, $collector.haystack,
                    );
                    $collector.evaluate_predicates_at_object(&resolver)?;
                    with_stack_room(|| {
                        for ($key, child) in $entries {
                            $collector.with_key($key_segment, |collector| {
                                collector.$method($($context,)? child)
                            })?;
                            if $collector.is_full() {
                                break;
                            }
                        }
                        Ok::<_, SearchError>(())
                    })?;
                }
                $leaf => {
                    if let Some((start, end)) = $span {
                        $collector.charge(end - start)?;
                    }
                }
            }
            Ok(())
        }
    };
}

/// Work charged per visited node, on top of its path and text bytes.
const NODE_WORK: usize = 16;
/// Work budget per document byte. Generous for normal documents, but bounds YAML alias
/// expansion, where a short document can stand for enormous text or paths.
const WORK_PER_BYTE: usize = 256;
/// Minimum work budget for any document.
const MIN_WORK: usize = 1 << 24;

pub(super) struct MatchCollector<'engine, 'haystack> {
    engine: &'engine StructuredEngine,
    haystack: &'haystack str,
    config: SearchConfig,
    record_nodes: bool,
    matches: Vec<StructuredMatch<'haystack>>,
    path: Vec<u8>,
    path_value: Vec<u8>,
    limit: usize,
    /// Path and scalar bytes handled so far, plus [`NODE_WORK`] per node.
    work: usize,
    work_budget: usize,
    value_occurrences: Option<ValueOccurrences>,
}

/// Where a single value pattern occurs in the searched text, found in one pass up front. A
/// value read straight from that text, rather than unescaped or normalized, contains the
/// pattern only if one of those occurrences lies inside it, which spares most values their
/// own search. Values that may contain it are still searched, so whole-word and folded
/// matching keep their rules.
struct ValueOccurrences {
    /// Starts of every occurrence, overlapping ones included, in order.
    starts: Vec<usize>,
    needle_len: usize,
    /// The first occurrence at or after the last value checked. Values come mostly in
    /// document order, so the next one usually needs a step or two forward from here.
    next: usize,
}

impl ValueOccurrences {
    /// Text bytes per occurrence below which the pattern is too common to be worth indexing.
    const MIN_SPACING: usize = 32;

    fn find(needle: &Needle, haystack: &str) -> Option<Self> {
        let bytes = haystack.as_bytes();
        let most = bytes.len() / Self::MIN_SPACING;
        let mut starts = Vec::new();
        let mut from = 0;
        while let Some(found) = bytes.get(from..).and_then(|rest| needle.find(rest)) {
            if starts.len() == most {
                return None;
            }
            starts.push(from + found);
            from += found + 1;
        }
        Some(Self {
            starts,
            needle_len: needle.as_bytes().len(),
            next: 0,
        })
    }

    /// Whether the pattern may occur in `value`: false only for a value lying in `haystack`
    /// with no occurrence inside it.
    fn may_occur_in(&mut self, haystack: &str, value: &str) -> bool {
        let Some(offset) = (value.as_ptr() as usize)
            .checked_sub(haystack.as_ptr() as usize)
            .filter(|offset| offset + value.len() <= haystack.len())
        else {
            return true;
        };
        let starts = &self.starts;
        let before = |index: usize| starts.get(index).is_some_and(|&start| start < offset);
        // Out-of-order values, such as a table's later subtables, fall back to a search.
        if self.next > 0 && !before(self.next - 1) {
            self.next = starts.partition_point(|&start| start < offset);
        }
        while before(self.next) {
            self.next += 1;
        }
        starts
            .get(self.next)
            .is_some_and(|&start| start + self.needle_len <= offset + value.len())
    }
}

impl<'engine, 'haystack> MatchCollector<'engine, 'haystack> {
    pub(super) fn new(
        engine: &'engine StructuredEngine,
        haystack: &'haystack str,
        config: SearchConfig,
        limit: usize,
    ) -> Self {
        let mut path = Vec::with_capacity(128);
        path.push(b'$');
        let value_occurrences = engine
            .value_matcher
            .as_ref()
            .and_then(PatternMatcher::prefilter_needle)
            .and_then(|needle| ValueOccurrences::find(needle, haystack));

        Self {
            engine,
            haystack,
            config,
            record_nodes: engine.path_matcher.is_some()
                || engine.value_matcher.is_some()
                || engine.path_value_matcher.is_some(),
            matches: Vec::with_capacity(limit.min(64)),
            path,
            path_value: Vec::with_capacity(128),
            limit,
            work: 0,
            work_budget: haystack
                .len()
                .saturating_mul(WORK_PER_BYTE)
                .saturating_add(MIN_WORK),
            value_occurrences,
        }
    }

    /// Adds `work` to the running total, failing with [`SearchError::StructuredParse`] once
    /// it exceeds the budget.
    fn charge(&mut self, work: usize) -> Result<(), SearchError> {
        self.work = self.work.saturating_add(work);
        if self.work <= self.work_budget {
            return Ok(());
        }
        Err(SearchError::StructuredParse {
            mode: self
                .config
                .structured_mode()
                .expect("structured searches are configured with a mode"),
            message: "aliases or nested keys repeat more text than a search can read".to_owned(),
        })
    }

    pub(super) fn finish(mut self, limit: usize) -> Vec<StructuredMatch<'haystack>> {
        self.matches.truncate(limit);
        self.matches
    }

    fn is_full(&self) -> bool {
        self.matches.len() >= self.limit
    }

    /// Records the matches of a node spanning `own`, whose text when matched is the span
    /// `full_span` returns for `own` in the haystack; it can cost more, so it is found only
    /// for a match.
    // Kept out of line so the recursive visits keep small stack frames.
    #[inline(never)]
    fn record_node(
        &mut self,
        own: (usize, usize),
        full_span: impl Fn((usize, usize), &str) -> (usize, usize),
        value_text: Option<Cow<'_, str>>,
    ) {
        let engine = self.engine;
        let haystack = self.haystack;
        let mut span = None;
        for (matcher, kind) in [
            (&engine.path_matcher, StructuredMatchKind::Path),
            (&engine.value_matcher, StructuredMatchKind::Value),
            (&engine.path_value_matcher, StructuredMatchKind::PathValue),
        ] {
            if self.is_full() {
                break;
            }
            let Some(matcher) = matcher else {
                continue;
            };
            let record_bytes = match kind {
                StructuredMatchKind::Path => self.path.as_slice(),
                StructuredMatchKind::Value | StructuredMatchKind::PathValue => {
                    let Some(value) = value_text.as_ref() else {
                        continue;
                    };
                    if kind == StructuredMatchKind::Value {
                        if self
                            .value_occurrences
                            .as_mut()
                            .is_some_and(|found| !found.may_occur_in(haystack, value))
                        {
                            continue;
                        }
                        value.as_bytes()
                    } else {
                        let named_len = self.path.len() + 1 + value.len();
                        if !engine.nodes_by_path_value.names_length(named_len) {
                            continue;
                        }
                        self.path_value.clear();
                        self.path_value.reserve(self.path.len() + 1 + value.len());
                        self.path_value.extend_from_slice(&self.path);
                        self.path_value.push(b'=');
                        self.path_value.extend_from_slice(value.as_bytes());
                        self.path_value.as_slice()
                    }
                }
                StructuredMatchKind::Predicate => continue,
            };
            let mut matched_patterns = SmallVec::<[usize; 4]>::new();
            match kind {
                // Path patterns must equal the node's whole path, not just occur in it.
                StructuredMatchKind::Path | StructuredMatchKind::PathValue => {
                    let nodes = if kind == StructuredMatchKind::Path {
                        &engine.nodes_by_path
                    } else {
                        &engine.nodes_by_path_value
                    };
                    matched_patterns.extend(nodes.get(record_bytes).into_iter().flatten().copied());
                }
                StructuredMatchKind::Value | StructuredMatchKind::Predicate => {
                    matcher.for_each_match(record_bytes, self.config, |index| {
                        matched_patterns.push(index)
                    });
                }
            }
            if matched_patterns.is_empty() {
                continue;
            }
            let span = *span.get_or_insert_with(|| full_span(own, haystack));
            self.push_matches(span, kind, matched_patterns);
        }
    }

    fn push_matches(
        &mut self,
        span: (usize, usize),
        match_kind: StructuredMatchKind,
        mut matched_patterns: SmallVec<[usize; 4]>,
    ) {
        let remaining = self.limit.saturating_sub(self.matches.len());
        matched_patterns.truncate(remaining);
        let Some(last_pattern_index) = matched_patterns.pop() else {
            return;
        };
        let path = self.current_path_string();
        let node_text = &self.haystack[span.0..span.1];
        // Charge for the path copies now; the budget is checked at the next node.
        let copied = path.len().saturating_mul(matched_patterns.len() + 1);
        self.work = self.work.saturating_add(copied);

        self.matches.reserve(matched_patterns.len() + 1);
        for pattern_index in matched_patterns {
            self.matches.push(make_structured_match(
                node_text,
                span,
                pattern_index,
                path.clone(),
                match_kind,
            ));
        }

        self.matches.push(make_structured_match(
            node_text,
            span,
            last_pattern_index,
            path,
            match_kind,
        ));
    }

    fn with_array_index<T>(&mut self, index: usize, visit: impl FnOnce(&mut Self) -> T) -> T {
        let original_len = self.path.len();
        append_array_segment(&mut self.path, index);
        let output = visit(self);
        self.path.truncate(original_len);
        output
    }

    fn with_key<T>(&mut self, key: &str, visit: impl FnOnce(&mut Self) -> T) -> T {
        let original_len = self.path.len();
        append_key_segment(&mut self.path, key.as_bytes());
        let output = visit(self);
        self.path.truncate(original_len);
        output
    }

    fn current_path_string(&self) -> String {
        String::from_utf8(self.path.clone()).expect("structured paths are always valid UTF-8")
    }

    fn predicate_path_string(&self, segments: &[PathSegment]) -> String {
        format_path_with_base(self.path.as_slice(), segments)
    }

    fn handle_predicate_error(
        &self,
        pattern_index: usize,
        path: String,
        error: PredicateTypeError,
    ) -> Result<(), SearchError> {
        match self.config.structured_predicate_handling() {
            StructuredPredicateHandling::Ignore => Ok(()),
            StructuredPredicateHandling::Error => Err(SearchError::StructuredPredicate {
                pattern_index,
                path,
                message: error.to_string(),
            }),
        }
    }

    fn evaluate_predicates_at_object(
        &mut self,
        resolver: &impl PredicateResolver,
    ) -> Result<(), SearchError> {
        for predicate in &self.engine.compiled_predicates {
            if self.is_full() {
                break;
            }
            match evaluate_compiled_predicate(resolver, predicate) {
                Ok(Some((start, end))) => {
                    let path = self.predicate_path_string(&predicate.clauses[0].segments);
                    self.matches.push(make_structured_match(
                        &self.haystack[start..end],
                        (start, end),
                        predicate.pattern_index,
                        path,
                        StructuredMatchKind::Predicate,
                    ));
                }
                Ok(None) => {}
                Err((segments, error)) => {
                    let path = self.predicate_path_string(segments);
                    self.handle_predicate_error(predicate.pattern_index, path, error)?;
                }
            }
        }
        Ok(())
    }

    visit_ast!(visit_json(self, [], node: &json_spanned::Value) {
        span: Some(node.span()), full_span: |span, _| span, scalar: json_scalar_text,
        context: &(),
        value: node.get_ref(),
        sequence: json_spanned_value::Value::Array(values) => values.iter().enumerate(),
        mapping: json_spanned_value::Value::Object(entries) => {
            let mut ordered = entries.iter().collect::<Vec<_>>();
            ordered.sort_unstable_by_key(|(_, child)| child.start());
            ordered
        },
        key: key => key.as_str(),
        leaf: json_spanned_value::Value::Null | json_spanned_value::Value::Bool(_)
            | json_spanned_value::Value::Number(_) | json_spanned_value::Value::String(_),
    });

    pub(super) fn visit_yaml_stream(&mut self, haystack: &str) -> Result<(), SearchError> {
        let mut parser = YamlParser::new();
        let mut bytes = haystack.as_bytes();
        parser.set_input_string(&mut bytes);

        let first = load_yaml_document(&mut parser)?;
        if first.nodes.is_empty() {
            return Ok(());
        }

        let second = load_yaml_document(&mut parser)?;
        if second.nodes.is_empty() {
            self.visit_yaml_document(&first)?;
            return Ok(());
        }

        // Load later documents lazily so a match limit never parses more than needed.
        let documents = [Ok(first), Ok(second)]
            .into_iter()
            .chain(std::iter::repeat_with(|| load_yaml_document(&mut parser)));
        for (index, document) in documents.enumerate() {
            let document = document?;
            if document.nodes.is_empty() {
                break;
            }
            self.with_array_index(index, |collector| collector.visit_yaml_document(&document))?;
            if self.is_full() {
                break;
            }
        }

        Ok(())
    }

    fn visit_yaml_document(&mut self, document: &YamlDocument) -> Result<(), SearchError> {
        if let Some(root) = document.nodes.first() {
            self.visit_yaml_node(document, root)?;
        }
        Ok(())
    }

    visit_ast!(visit_yaml_node(self, [document: &YamlDocument], node: &YamlNode) {
        span: yaml_document_byte_range(node), full_span: |span, _| span,
        scalar: yaml_document_scalar_text, context: document, value: &node.data,
        sequence: YamlNodeData::Sequence { items, .. } => items.iter().enumerate()
            .filter_map(|(index, &id)| document.get_node(id).map(|child| (index, child))),
        mapping: YamlNodeData::Mapping { pairs, .. } =>
            yaml_mapping_entries(document, pairs, self.haystack),
        key: key => key,
        leaf: YamlNodeData::Scalar { .. } | YamlNodeData::NoNode,
    });

    // toml-spanner keeps entries in source order, so they need no sorting. A table's text
    // can span its subtables, which takes a walk over them, so only matches find it.
    visit_ast!(visit_toml(self, [], node: &TomlItem<'_>) {
        span: Some((node.span().start as usize, node.span().end as usize)),
        full_span: |_, haystack| toml_item_span(node, haystack), scalar: toml_scalar_text,
        context: &(),
        value: node.value(),
        sequence: TomlValue::Array(values) => values.iter().enumerate(),
        mapping: TomlValue::Table(entries) => entries,
        key: key => key.name,
        leaf: TomlValue::String(_) | TomlValue::Integer(_) | TomlValue::Float(_)
            | TomlValue::Boolean(_) | TomlValue::DateTime(_),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_occurrences_overlap_and_skip_patterns_too_common_to_index() {
        let haystack = format!("aaa{}", " ".repeat(125)).repeat(4);

        let found = ValueOccurrences::find(&Needle::new(b"aa", false), &haystack);
        let too_common = ValueOccurrences::find(&Needle::new(b" ", false), &haystack);

        let starts = found.map(|found| found.starts);
        assert_eq!(starts, Some(vec![0, 1, 128, 129, 256, 257, 384, 385]));
        assert!(too_common.is_none());
    }

    #[test]
    fn values_in_the_text_may_hold_the_pattern_only_around_an_occurrence() {
        let haystack = format!("{}x = \"cat\"\ny = \"dog\"\nz = \"cat\"\n", " ".repeat(64));
        let mut found =
            ValueOccurrences::find(&Needle::new(b"cat", false), &haystack).expect("rare enough");
        let value = |text: &str| {
            let start = haystack.find(text).expect("the text holds it");
            &haystack[start..start + text.len()]
        };
        let owned = String::from("cat");

        // In order, backwards, then outside the text, which is always searched.
        assert!(found.may_occur_in(&haystack, value("cat\"\ny")));
        assert!(!found.may_occur_in(&haystack, value("dog")));
        assert!(found.may_occur_in(&haystack, value("cat\"\n")));
        assert!(!found.may_occur_in(&haystack, value("ca")));
        assert!(!found.may_occur_in(&haystack, value("dog")));
        assert!(found.may_occur_in(&haystack, &owned));
    }
}
