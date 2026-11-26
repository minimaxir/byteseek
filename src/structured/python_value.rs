//! Reads a structured match's `node_text` into the Python value it means, reading scalars as
//! byteseek's predicates do.
//!
//! Values are built without recursion, so a deeply nested document costs heap rather than the
//! calling thread's stack.

use std::fmt;
use std::sync::Arc;

use libyaml_safer::{NodeData as YamlNodeData, ScalarStyle as YamlScalarStyle};
use pyo3::IntoPyObjectExt;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDate, PyDateTime, PyDelta, PyDict, PyList, PyString, PyTime, PyTzInfo};
use serde::de::{DeserializeSeed, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use toml_spanner::{
    DateTime as TomlDateTime, Item as TomlItem, TimeOffset as TomlTimeOffset, Value as TomlValue,
};

use super::predicate::parse_numeric_text;
use super::value::{
    PlainYamlScalarKind, classify_yaml_plain_scalar, load_yaml_document, yaml_mapping_entries,
};
use super::with_stack_room;
use crate::json_scanner::{PathSegment, parse_path_pattern};
use crate::structured_predicate::Number;
use crate::{SearchError, StructuredMode};

/// The text a TOML match was found in.
///
/// A table written with headers or dotted keys names itself from the table holding it, and
/// text starting its line with `[` may be a header or an element of an array that spans
/// lines, so their `node_text` can't be read alone. They are read from this document by their
/// path instead, which TOML's unique keys make exact.
#[derive(Debug)]
pub(crate) enum TomlSource {
    /// The `str` given to `find_structured`.
    Text(Py<PyString>),
    /// A file's contents, shared by the matches found in it.
    File(Arc<str>),
}

impl TomlSource {
    fn clone_ref(&self, py: Python<'_>) -> Self {
        match self {
            Self::Text(text) => Self::Text(text.clone_ref(py)),
            Self::File(contents) => Self::File(Arc::clone(contents)),
        }
    }

    fn text<'a>(&'a self, py: Python<'a>) -> PyResult<&'a str> {
        match self {
            Self::Text(text) => text.bind(py).to_str(),
            Self::File(contents) => Ok(contents),
        }
    }
}

/// What reading a match's `node_text` needs from the text around it.
#[derive(Debug)]
pub(crate) enum NodeContext {
    /// The text reads as a value by itself.
    Standalone,
    /// A YAML node spanning lines whose first line starts this many characters into its
    /// line. The lines after it keep their indentation, so the first line needs it back.
    YamlColumn(u32),
    /// A TOML node, with the document it was found in.
    Toml {
        source: TomlSource,
        /// Whether the text starts its line, as headers and arrays of tables do.
        starts_line: bool,
    },
}

impl NodeContext {
    /// Finds what the node at `start` in `haystack`, written as `node_text`, needs to be read.
    /// `toml_source` gives the searched document, and is called only for TOML.
    pub(crate) fn of(
        mode: StructuredMode,
        haystack: &str,
        start: usize,
        node_text: &str,
        toml_source: impl FnOnce() -> TomlSource,
    ) -> Self {
        let before = &haystack.as_bytes()[..start];
        match mode {
            StructuredMode::Json => Self::Standalone,
            StructuredMode::Yaml if memchr::memchr(b'\n', node_text.as_bytes()).is_some() => {
                let line_start = memchr::memrchr(b'\n', before).map_or(0, |newline| newline + 1);
                match haystack[line_start..start].chars().count() {
                    0 => Self::Standalone,
                    column => u32::try_from(column).map_or(Self::Standalone, Self::YamlColumn),
                }
            }
            StructuredMode::Yaml => Self::Standalone,
            StructuredMode::Toml => Self::Toml {
                source: toml_source(),
                starts_line: before
                    .iter()
                    .rev()
                    .find(|&&byte| !matches!(byte, b' ' | b'\t'))
                    .is_none_or(|&byte| byte == b'\n'),
            },
        }
    }

    /// Copies the context for another match object, sharing its document.
    pub(crate) fn clone_ref(&self, py: Python<'_>) -> Self {
        match self {
            Self::Standalone => Self::Standalone,
            Self::YamlColumn(column) => Self::YamlColumn(*column),
            Self::Toml {
                source,
                starts_line,
            } => Self::Toml {
                source: source.clone_ref(py),
                starts_line: *starts_line,
            },
        }
    }
}

/// Reads `node_text`, found at `path` in a document of format `mode`, into Python objects.
///
/// # Errors
///
/// Returns `ValueError` when the text doesn't read as a value of its format, such as a YAML
/// alias whose anchor lies outside it, or JSON nested more than 128 levels deep.
pub(crate) fn node_value<'py>(
    py: Python<'py>,
    mode: StructuredMode,
    node_text: &str,
    path: &str,
    context: &NodeContext,
) -> PyResult<Bound<'py, PyAny>> {
    // The JSON and TOML parsers recurse per nesting level.
    with_stack_room(|| match (mode, context) {
        (StructuredMode::Json, _) => json_value(py, node_text),
        (StructuredMode::Yaml, NodeContext::YamlColumn(column)) => {
            yaml_value(py, node_text, *column)
        }
        (StructuredMode::Yaml, _) => yaml_value(py, node_text, 0),
        (
            StructuredMode::Toml,
            NodeContext::Toml {
                source,
                starts_line,
            },
        ) => toml_value(py, node_text, path, *starts_line, source),
        (StructuredMode::Toml, NodeContext::Standalone | NodeContext::YamlColumn(_)) => Err(
            unreadable(StructuredMode::Toml, "the searched document is unavailable"),
        ),
    })
}

fn unreadable(mode: StructuredMode, reason: impl fmt::Display) -> PyErr {
    let format = match mode {
        StructuredMode::Json => "JSON",
        StructuredMode::Yaml => "YAML",
        StructuredMode::Toml => "TOML",
    };
    PyValueError::new_err(format!(
        "node_text does not read as a {format} value: {reason}"
    ))
}

/// A node's Python value, or the children of the `list` or `dict` it becomes.
enum Expanded<'py, 'k, N> {
    Value(Bound<'py, PyAny>),
    List(Vec<N>),
    Dict(Vec<(&'k str, N)>),
}

/// An open `list` or `dict` with the children it has left to build.
enum OpenContainer<'py, 'k, N> {
    List(Bound<'py, PyList>, std::vec::IntoIter<N>),
    /// The key is that of the child being built.
    Dict(
        Bound<'py, PyDict>,
        std::vec::IntoIter<(&'k str, N)>,
        Option<&'k str>,
    ),
}

/// Builds the Python value of `root` depth first with a heap stack of open containers,
/// expanding each node with `expand`.
fn build_python<'py, 'k, N>(
    py: Python<'py>,
    root: N,
    mut expand: impl FnMut(N) -> PyResult<Expanded<'py, 'k, N>>,
) -> PyResult<Bound<'py, PyAny>> {
    let mut open: Vec<OpenContainer<'py, 'k, N>> = Vec::new();
    let mut node = root;
    loop {
        let mut finished = match expand(node)? {
            Expanded::Value(value) => Some(value),
            Expanded::List(items) => {
                open.push(OpenContainer::List(PyList::empty(py), items.into_iter()));
                None
            }
            Expanded::Dict(entries) => {
                open.push(OpenContainer::Dict(
                    PyDict::new(py),
                    entries.into_iter(),
                    None,
                ));
                None
            }
        };
        // Add finished values to their containers, closing each container whose children are
        // all built, until a child is left to build.
        node = loop {
            let Some(container) = open.last_mut() else {
                // Only the root finishes with no container open.
                return Ok(finished.unwrap_or_else(|| py.None().into_bound(py)));
            };
            let next = match container {
                OpenContainer::List(list, items) => {
                    if let Some(value) = finished.take() {
                        list.append(value)?;
                    }
                    items.next()
                }
                OpenContainer::Dict(dict, entries, key) => {
                    if let (Some(value), Some(key)) = (finished.take(), key.take()) {
                        dict.set_item(key, value)?;
                    }
                    entries.next().map(|(next_key, child)| {
                        *key = Some(next_key);
                        child
                    })
                }
            };
            if let Some(next) = next {
                break next;
            }
            finished = open.pop().map(|closed| match closed {
                OpenContainer::List(list, _) => list.into_any(),
                OpenContainer::Dict(dict, ..) => dict.into_any(),
            });
        };
    }
}

fn json_value<'py>(py: Python<'py>, text: &str) -> PyResult<Bound<'py, PyAny>> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    PythonValue(py)
        .deserialize(&mut deserializer)
        .and_then(|value| deserializer.end().map(|()| value))
        .map_err(|error| unreadable(StructuredMode::Json, error))
}

/// Builds Python objects as serde_json parses, keeping object keys in document order.
/// serde_json stops at 128 levels of nesting, which bounds this recursion.
#[derive(Clone, Copy)]
struct PythonValue<'py>(Python<'py>);

impl<'py> PythonValue<'py> {
    fn object<E: serde::de::Error>(
        self,
        value: impl IntoPyObject<'py>,
    ) -> Result<Bound<'py, PyAny>, E> {
        value.into_bound_py_any(self.0).map_err(E::custom)
    }
}

impl<'de, 'py> DeserializeSeed<'de> for PythonValue<'py> {
    type Value = Bound<'py, PyAny>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de, 'py> Visitor<'de> for PythonValue<'py> {
    type Value = Bound<'py, PyAny>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(self.0.None().into_bound(self.0))
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
        self.object(value)
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
        self.object(value)
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
        self.object(value)
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
        self.object(value)
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        self.object(value)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Self::Value, A::Error> {
        let list = PyList::empty(self.0);
        while let Some(item) = items.next_element_seed(self)? {
            list.append(item).map_err(A::Error::custom)?;
        }
        Ok(list.into_any())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Self::Value, A::Error> {
        let dict = PyDict::new(self.0);
        while let Some(key) = entries.next_key_seed(self)? {
            let value = entries.next_value_seed(self)?;
            dict.set_item(key, value).map_err(A::Error::custom)?;
        }
        Ok(dict.into_any())
    }
}

fn yaml_value<'py>(py: Python<'py>, text: &str, column: u32) -> PyResult<Bound<'py, PyAny>> {
    let indented = format!("{:width$}{text}", "", width = column as usize);
    let mut parser = libyaml_safer::Parser::new();
    let mut bytes = indented.as_bytes();
    parser.set_input_string(&mut bytes);
    // Loading checks nesting depth and alias expansion, which bound the values built here.
    let document = load_yaml_document(&mut parser).map_err(|error| match error {
        SearchError::StructuredParse { message, .. } => unreadable(StructuredMode::Yaml, message),
        other => unreadable(StructuredMode::Yaml, other),
    })?;
    let Some(root) = document.nodes.first() else {
        return Ok(py.None().into_bound(py));
    };
    build_python(py, root, |node| {
        Ok(match &node.data {
            YamlNodeData::Scalar { value, style } => {
                Expanded::Value(yaml_scalar(py, value, *style)?)
            }
            YamlNodeData::Sequence { items, .. } => Expanded::List(
                items
                    .iter()
                    .filter_map(|&id| document.get_node(id))
                    .collect(),
            ),
            YamlNodeData::Mapping { pairs, .. } => {
                Expanded::Dict(yaml_mapping_entries(&document, pairs, &indented).collect())
            }
            YamlNodeData::NoNode => Expanded::Value(py.None().into_bound(py)),
        })
    })
}

fn yaml_scalar<'py>(
    py: Python<'py>,
    value: &str,
    style: YamlScalarStyle,
) -> PyResult<Bound<'py, PyAny>> {
    match classify_yaml_plain_scalar(value, style) {
        PlainYamlScalarKind::Null => Ok(py.None().into_bound(py)),
        PlainYamlScalarKind::Bool(flag) => flag.into_bound_py_any(py),
        PlainYamlScalarKind::Number => match parse_numeric_text(value) {
            Ok(Number::Integer(number)) => number.into_bound_py_any(py),
            Ok(Number::Float(number)) => number.into_bound_py_any(py),
            // Predicates can't compare it as a number either, so it stays text.
            Err(_) => value.into_bound_py_any(py),
        },
        PlainYamlScalarKind::String => value.into_bound_py_any(py),
    }
}

fn toml_value<'py>(
    py: Python<'py>,
    text: &str,
    path: &str,
    starts_line: bool,
    source: &TomlSource,
) -> PyResult<Bound<'py, PyAny>> {
    let arena = toml_spanner::Arena::new();
    // Text written after a key, or as an array element, reads after one. Table text, such as
    // dotted keys in an inline table, doesn't, and falls through to the document.
    if !(starts_line && text.starts_with('[')) {
        let assignment = format!("v = {text}");
        if let Ok(document) = toml_spanner::parse(&assignment, &arena) {
            let table = document.table();
            if let Some(item) = table.get("v").filter(|_| table.len() == 1) {
                return toml_item(py, item);
            }
        }
    }
    let document = toml_spanner::parse(source.text(py)?, &arena)
        .map_err(|error| unreadable(StructuredMode::Toml, error))?;
    let segments = parse_path_pattern(path.as_bytes(), 0)
        .ok_or_else(|| unreadable(StructuredMode::Toml, "its path doesn't parse"))?
        .segments;
    let item = segments
        .iter()
        .try_fold(document.table().as_item(), |item, segment| match segment {
            PathSegment::Key(key) => item.as_table()?.get(key),
            PathSegment::Index(index) => item.as_array()?.get(*index),
        })
        .ok_or_else(|| unreadable(StructuredMode::Toml, "its path isn't in the document"))?;
    toml_item(py, item)
}

fn toml_item<'py>(py: Python<'py>, item: &TomlItem<'_>) -> PyResult<Bound<'py, PyAny>> {
    build_python(py, item, |item| {
        Ok(match item.value() {
            TomlValue::String(text) => Expanded::Value(text.into_bound_py_any(py)?),
            TomlValue::Integer(integer) => {
                Expanded::Value(integer.as_i128().into_bound_py_any(py)?)
            }
            TomlValue::Float(number) => Expanded::Value(number.into_bound_py_any(py)?),
            TomlValue::Boolean(flag) => Expanded::Value(flag.into_bound_py_any(py)?),
            TomlValue::DateTime(datetime) => Expanded::Value(toml_datetime(py, datetime)?),
            TomlValue::Array(items) => Expanded::List(items.iter().collect()),
            TomlValue::Table(table) => Expanded::Dict(
                table
                    .entries()
                    .iter()
                    .map(|(key, child)| (key.name, child))
                    .collect(),
            ),
        })
    })
}

/// Converts a TOML datetime to `datetime`, `date`, or `time`, with a fixed `tzinfo` when it
/// has an offset. Python keeps microseconds, so finer fractions are truncated, and a value
/// Python can't hold, such as year 0 or a leap second, raises `ValueError`.
fn toml_datetime<'py>(py: Python<'py>, datetime: &TomlDateTime) -> PyResult<Bound<'py, PyAny>> {
    let tzinfo = match datetime.offset() {
        None => None,
        Some(TomlTimeOffset::Z) => Some(PyTzInfo::utc(py)?.to_owned()),
        Some(TomlTimeOffset::Custom { minutes }) => Some(PyTzInfo::fixed_offset(
            py,
            PyDelta::new(py, 0, i32::from(minutes) * 60, 0, true)?,
        )?),
    };
    match (datetime.date(), datetime.time()) {
        (Some(date), Some(time)) => Ok(PyDateTime::new(
            py,
            i32::from(date.year),
            date.month,
            date.day,
            time.hour,
            time.minute,
            time.second,
            time.nanosecond / 1_000,
            tzinfo.as_ref(),
        )?
        .into_any()),
        (Some(date), None) => {
            Ok(PyDate::new(py, i32::from(date.year), date.month, date.day)?.into_any())
        }
        (None, time) => {
            let time = time.ok_or_else(|| unreadable(StructuredMode::Toml, "an empty datetime"))?;
            Ok(PyTime::new(
                py,
                time.hour,
                time.minute,
                time.second,
                time.nanosecond / 1_000,
                tzinfo.as_ref(),
            )?
            .into_any())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context_at(mode: StructuredMode, haystack: &str, node_text: &str) -> NodeContext {
        let start = haystack
            .find(node_text)
            .expect("the node is in the haystack");
        NodeContext::of(mode, haystack, start, node_text, || {
            TomlSource::File(Arc::from(haystack))
        })
    }

    fn starts_line(context: &NodeContext) -> Option<bool> {
        match context {
            NodeContext::Toml { starts_line, .. } => Some(*starts_line),
            NodeContext::Standalone | NodeContext::YamlColumn(_) => None,
        }
    }

    #[test]
    fn yaml_nodes_spanning_lines_keep_their_column_in_characters() {
        let haystack = "spec:\n  - ключ: 1\n    b: 2\n";
        let column = |node_text| match context_at(StructuredMode::Yaml, haystack, node_text) {
            NodeContext::YamlColumn(column) => Some(column),
            NodeContext::Standalone | NodeContext::Toml { .. } => None,
        };

        assert_eq!(column("ключ: 1\n    b: 2\n"), Some(4));
        assert_eq!(column("1"), None);
        assert_eq!(column("spec:\n  - ключ"), None);
    }

    #[test]
    fn toml_text_starting_its_line_is_marked() {
        let haystack = "a = [\n  [\"x\"],\n]\n  [t]\nk = 1\n";
        let marked =
            |node_text| starts_line(&context_at(StructuredMode::Toml, haystack, node_text));

        assert_eq!(marked("[t]\nk = 1"), Some(true));
        assert_eq!(marked("[\"x\"]"), Some(true));
        assert_eq!(marked("[\n  [\"x\"],\n]"), Some(false));
        assert_eq!(
            starts_line(&context_at(StructuredMode::Json, "{\"a\":\n[1]}", "[1]")),
            None
        );
    }
}
