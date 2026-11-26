use libyaml_safer::{
    EventData as YamlEventData, Parser as YamlParser, ScalarStyle as YamlScalarStyle,
};

use crate::jq_path::{append_array_segment, append_key_segment};
use crate::json_scanner::PathSegment;
use crate::{SearchError, StructuredMode};

/// The innermost open collection on the way to the target.
enum YamlCursor {
    /// A mapping; `value_on_path` records whether the last key matched the next segment.
    Mapping {
        expecting_key: bool,
        value_on_path: bool,
    },
    Sequence {
        index: usize,
    },
}

/// Walks libyaml events of the first document to find the span of the value at `segments`.
/// Subtrees off the path are skipped by depth, and the walk ends once the collection holding
/// the next segment closes without it. Returns `None` at a merge key on the path, which only
/// the full parser applies.
pub(super) fn yaml_event_navigate(
    haystack: &str,
    segments: &[PathSegment],
) -> Option<Result<Option<(usize, usize)>, SearchError>> {
    let mut parser = YamlParser::new();
    let mut bytes = haystack.as_bytes();
    parser.set_input_string(&mut bytes);

    let mut cursor: Option<YamlCursor> = None;
    let mut matched = 0_usize;
    let mut skip_depth = 0_usize;
    // The target collection's start and nesting depth, once it opens.
    let mut target: Option<(usize, usize)> = None;

    loop {
        let event = match parser.parse() {
            Ok(event) => event,
            Err(error) => {
                return Some(Err(SearchError::StructuredParse {
                    mode: StructuredMode::Yaml,
                    message: error.to_string(),
                }));
            }
        };
        let start = usize::try_from(event.start_mark.index).unwrap_or(0);
        let end = usize::try_from(event.end_mark.index).unwrap_or(start);
        let opens = match &event.data {
            YamlEventData::MappingStart { .. } | YamlEventData::SequenceStart { .. } => true,
            YamlEventData::MappingEnd | YamlEventData::SequenceEnd => {
                match (&mut target, skip_depth) {
                    (Some((target_start, 1)), _) => return Some(Ok(Some((*target_start, end)))),
                    (Some((_, depth)), _) => *depth -= 1,
                    (None, 0) => return Some(Ok(None)),
                    (None, _) => skip_depth -= 1,
                }
                continue;
            }
            YamlEventData::Scalar { .. } | YamlEventData::Alias { .. } => false,
            YamlEventData::StreamEnd | YamlEventData::DocumentEnd { .. } => return Some(Ok(None)),
            YamlEventData::StreamStart { .. } | YamlEventData::DocumentStart { .. } => continue,
        };
        if let Some((_, depth)) = &mut target {
            *depth += usize::from(opens);
            continue;
        }
        if skip_depth > 0 {
            skip_depth += usize::from(opens);
            continue;
        }

        let on_path = match &mut cursor {
            None => true,
            Some(YamlCursor::Mapping {
                expecting_key: expecting_key @ true,
                value_on_path,
            }) => {
                if is_yaml_merge_event(&event.data) {
                    return None;
                }
                *expecting_key = false;
                *value_on_path = match (&event.data, &segments[matched]) {
                    (YamlEventData::Scalar { value, .. }, PathSegment::Key(key)) => value == key,
                    _ => false,
                };
                skip_depth = usize::from(opens);
                continue;
            }
            Some(YamlCursor::Mapping {
                expecting_key,
                value_on_path,
            }) => {
                *expecting_key = true;
                std::mem::take(value_on_path)
            }
            Some(YamlCursor::Sequence { index }) => {
                let on_path = matches!(segments[matched], PathSegment::Index(i) if i == *index);
                *index += 1;
                on_path
            }
        };
        if !on_path {
            skip_depth = usize::from(opens);
            continue;
        }
        if cursor.is_some() {
            matched += 1;
        }
        if matched == segments.len() {
            if !opens {
                return Some(Ok(Some((start, end))));
            }
            target = Some((start, 1));
            continue;
        }
        cursor = Some(match &event.data {
            YamlEventData::MappingStart { .. } => YamlCursor::Mapping {
                expecting_key: true,
                value_on_path: false,
            },
            YamlEventData::SequenceStart { .. } => YamlCursor::Sequence { index: 0 },
            // A scalar has no children to hold the remaining segments.
            _ => return Some(Ok(None)),
        });
    }
}

/// Whether a key event is YAML's merge key, a plain `<<`.
pub(super) fn is_yaml_merge_event(data: &YamlEventData) -> bool {
    matches!(data, YamlEventData::Scalar { value, style: YamlScalarStyle::Plain, .. } if value == "<<")
}

/// Builds a jq-style path string such as `$.events[0].name`.
pub(super) fn format_path_from_segments(segments: &[PathSegment]) -> String {
    format_path_with_base(b"$", segments)
}

pub(super) fn format_path_with_base(base: &[u8], segments: &[PathSegment]) -> String {
    let mut path = Vec::with_capacity(base.len() + segments.len() * 8 + 8);
    path.extend_from_slice(base);
    for segment in segments {
        match segment {
            PathSegment::Key(key) => append_key_segment(&mut path, key.as_bytes()),
            PathSegment::Index(index) => append_array_segment(&mut path, *index),
        }
    }
    String::from_utf8(path).expect("structured paths are always valid UTF-8")
}
