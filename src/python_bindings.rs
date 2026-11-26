use std::ffi::OsString;
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pyo3::IntoPyObjectExt;
use pyo3::create_exception;
use pyo3::exceptions::{PyFileNotFoundError, PyValueError};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyInt, PyList, PyModule, PyString, PyTuple};
use smallvec::SmallVec;

use crate::structured::python_value::{NodeContext, TomlSource, node_value};
use crate::{
    CaseMode, Match, OwnedStructuredMatch, PathOrder, SearchConfig, Searcher, StructuredMatch,
    StructuredMatchKind, StructuredMode, StructuredPredicateHandling,
};

// Named for the re-exporting package so tracebacks and `pickle` see `byteseek.SearchError`.
// A `ValueError`, as `json.JSONDecodeError` is: bad patterns and unparsable documents are
// both bad values.
create_exception!(
    byteseek,
    SearchError,
    PyValueError,
    "Raised when patterns or options are invalid, or a searched text or file can't be read."
);

/// `errno.ENOENT`, which is 2 on every platform CPython supports.
const ENOENT: i32 = 2;

/// A `byteseek._enums` class with its members, paired with the Rust variants they stand for.
struct EnumClass<T: 'static> {
    class: Py<PyAny>,
    members: Vec<(T, Py<PyAny>)>,
}

/// A Rust enum mirrored by the `enum.StrEnum` of the same name in `byteseek._enums`.
trait PythonEnum: Copy + PartialEq + Send + Sync + 'static {
    /// The Python class name.
    const CLASS: &'static str;
    /// Every variant with its Python member's name.
    const MEMBERS: &'static [(Self, &'static str)];

    /// The class, imported on first use: the extension module loads before the package.
    fn cache() -> &'static PyOnceLock<EnumClass<Self>>;

    fn class(py: Python<'_>) -> PyResult<&'static EnumClass<Self>> {
        Self::cache().get_or_try_init(py, || {
            let class = py
                .import(intern!(py, "byteseek._enums"))?
                .getattr(Self::CLASS)?;
            let members = Self::MEMBERS
                .iter()
                .map(|&(variant, name)| Ok((variant, class.getattr(name)?.unbind())))
                .collect::<PyResult<_>>()?;
            Ok(EnumClass {
                class: class.unbind(),
                members,
            })
        })
    }

    fn to_python(self, py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
        let (_, member) = Self::class(py)?
            .members
            .iter()
            .find(|(variant, _)| *variant == self)
            .expect("every variant has a Python member");
        Ok(member.bind(py).clone())
    }

    /// Reads a member, or a string equal to one's value as `enum.StrEnum` allows.
    fn from_python(object: &Bound<'_, PyAny>) -> PyResult<Self> {
        let class = Self::class(object.py())?;
        // Calling the class maps a value to its member and raises `ValueError` for others.
        let member = class.class.bind(object.py()).call1((object,))?;
        Ok(class
            .members
            .iter()
            .find(|(_, candidate)| member.is(candidate))
            .map(|&(variant, _)| variant)
            .expect("an enum class returns one of its own members"))
    }
}

macro_rules! python_enum {
    ($rust:ident { $($variant:ident => $member:literal),+ $(,)? }) => {
        // Stops compiling when a Rust variant has no Python member.
        const _: fn($rust) = |value| match value {
            $($rust::$variant)|+ => {}
        };

        impl PythonEnum for $rust {
            const CLASS: &'static str = stringify!($rust);
            const MEMBERS: &'static [(Self, &'static str)] = &[$(($rust::$variant, $member)),+];

            fn cache() -> &'static PyOnceLock<EnumClass<Self>> {
                static CACHE: PyOnceLock<EnumClass<$rust>> = PyOnceLock::new();
                &CACHE
            }
        }

        impl<'py> IntoPyObject<'py> for $rust {
            type Target = PyAny;
            type Output = Bound<'py, PyAny>;
            type Error = PyErr;

            fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
                self.to_python(py)
            }
        }

        impl<'a, 'py> FromPyObject<'a, 'py> for $rust {
            type Error = PyErr;

            fn extract(object: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
                Self::from_python(&object)
            }
        }

        impl PythonLiteral for $rust {
            fn literal(&self) -> String {
                let (_, name) = Self::MEMBERS
                    .iter()
                    .find(|(variant, _)| variant == self)
                    .expect("every variant has a Python member");
                format!("{}.{name}", Self::CLASS)
            }
        }
    };
}

python_enum!(CaseMode {
    Sensitive => "SENSITIVE",
    AsciiInsensitive => "ASCII_INSENSITIVE",
});
python_enum!(PathOrder {
    Discovery => "DISCOVERY",
    Path => "PATH",
});
python_enum!(StructuredMode {
    Json => "JSON",
    Yaml => "YAML",
    Toml => "TOML",
});
python_enum!(StructuredPredicateHandling {
    Ignore => "IGNORE",
    Error => "ERROR",
});
python_enum!(StructuredMatchKind {
    Path => "PATH",
    Value => "VALUE",
    PathValue => "PATH_VALUE",
    Predicate => "PREDICATE",
});

/// Python source text for a configuration value, as `repr()` would write it.
trait PythonLiteral {
    fn literal(&self) -> String;
}

impl PythonLiteral for bool {
    fn literal(&self) -> String {
        if *self { "True" } else { "False" }.to_owned()
    }
}

impl PythonLiteral for usize {
    fn literal(&self) -> String {
        self.to_string()
    }
}

impl<T: PythonLiteral> PythonLiteral for Option<T> {
    fn literal(&self) -> String {
        self.as_ref()
            .map_or_else(|| "None".to_owned(), PythonLiteral::literal)
    }
}

// Result classes are frozen and store ready-made Python objects, which PyO3 exposes as member
// descriptors, so attribute reads convert nothing. Matches stay Rust values and become a tuple
// on each read, since a stored tuple per result adds work for the cyclic garbage collector.
// The `fields` listed after `compare` define equality, hashing, `repr()`, and `match`.
macro_rules! python_result {
    ($python:ident, $name:literal, $doc:literal,
     from($($argument:ident: $argument_type:ty),*)
     fields {
         $($(#[doc = $field_doc:literal])* $([$getter:ident])? $field:ident: $field_type:ty
             = $convert:expr;)*
     }
     compare [$($compared:ident),+]
     $(matches($matches_self:ident, $matches_py:ident) $matches:expr;)?
     $(methods { $($method:item)* })?) => {
        #[doc = $doc]
        #[pyclass(frozen, name = $name, module = "byteseek")]
        #[derive(Debug)]
        struct $python {
            $($(#[doc = $field_doc])* $(#[pyo3($getter)])? $field: $field_type,)*
        }

        impl $python {
            const COMPARED: &[&str] = &[$(stringify!($compared)),+];

            fn from_rust($($argument: $argument_type),*) -> Self {
                Self {
                    $($field: $convert,)*
                }
            }
        }

        #[pymethods]
        impl $python {
            #[classattr]
            fn __match_args__(py: Python<'_>) -> PyResult<Bound<'_, PyTuple>> {
                PyTuple::new(py, Self::COMPARED.iter().copied())
            }

            fn __repr__(slf: &Bound<'_, Self>) -> PyResult<String> {
                result_repr(slf.as_any(), $name, Self::COMPARED)
            }

            fn __eq__(slf: &Bound<'_, Self>, other: &Bound<'_, Self>) -> PyResult<bool> {
                compared_fields(slf.as_any(), Self::COMPARED)?
                    .eq(compared_fields(other.as_any(), Self::COMPARED)?)
            }

            fn __hash__(slf: &Bound<'_, Self>) -> PyResult<isize> {
                compared_fields(slf.as_any(), Self::COMPARED)?.hash()
            }

            $(
                /// The matches, in the order they were found.
                #[getter]
                fn matches<'py>(&$matches_self, $matches_py: Python<'py>)
                    -> PyResult<Bound<'py, PyTuple>>
                {
                    $matches
                }
            )?

            $($($method)*)?
        }
    };
}

/// The values of `fields` on `result`, which define its equality and hash.
fn compared_fields<'py>(
    result: &Bound<'py, PyAny>,
    fields: &[&str],
) -> PyResult<Bound<'py, PyTuple>> {
    let values = fields
        .iter()
        .map(|field| result.getattr(*field))
        .collect::<PyResult<Vec<_>>>()?;
    PyTuple::new(result.py(), values)
}

/// Writes `Name(field=value, ...)`, summarizing `matches` by its length, which may be large.
fn result_repr(result: &Bound<'_, PyAny>, name: &str, fields: &[&str]) -> PyResult<String> {
    let fields = fields
        .iter()
        .map(|&field| {
            let value = result.getattr(field)?;
            Ok(if field == "matches" {
                let count = value.len()?;
                format!(
                    "matches=<{count} {}>",
                    if count == 1 { "match" } else { "matches" }
                )
            } else {
                format!("{field}={}", value.repr()?)
            })
        })
        .collect::<PyResult<Vec<_>>>()?;
    Ok(format!("{name}({})", fields.join(", ")))
}

python_result! {
    PyMatch, "Match",
    "One match. `start` and `end` are UTF-8 byte offsets into the searched text, not `str` \
     indices: slice `text.encode()[match.start:match.end]` for non-ASCII text.",
    from(py: Python<'_>, value: Match, indexes: &[Py<PyInt>])
    fields {
        /// Byte offset where the match starts.
        [get] start: Py<PyInt> = python_int(py, value.start);
        /// Byte offset just past the match's end.
        [get] end: Py<PyInt> = python_int(py, value.end);
        /// Position in `Searcher.patterns` of the pattern that matched.
        [get] pattern_index: Py<PyInt> = pattern_index(py, indexes, value.pattern_index);
    }
    compare [start, end, pattern_index]
}

python_result! {
    PyLineMatch, "LineMatch",
    "A line holding at least one match. Offsets in `matches` are UTF-8 byte offsets into `line`.",
    from(py: Python<'_>, value: crate::LineMatch<'_>)
    fields {
        /// The line's 1-based number.
        [get] line_number: Py<PyInt> = python_int(py, value.line_number);
        /// The line without its line ending.
        [get] line: Py<PyString> = PyString::new(py, value.line).unbind();
        matches: SmallVec<[Match; 2]> = value.matches;
    }
    compare [line_number, line, matches]
    matches(self, py) match_tuple(py, self.matches.iter().copied());
}

python_result! {
    PyFileMatchResult, "FileMatchResult",
    "The matches in one file. Offsets in `matches` are UTF-8 byte offsets into the file.",
    from(py: Python<'_>, value: crate::FileMatchResult)
    fields {
        /// The file's path, decoded as `os.fsdecode` does.
        [get] path: Py<PyString> = path_text(py, &value.path);
        matches: Vec<Match> = value.matches;
    }
    compare [path, matches]
    matches(self, py) match_tuple(py, self.matches.iter().copied());
}

/// The fields of a structured match that borrows its text or owns it, borrowed so that
/// neither is copied on its way to Python.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StructuredMatchFields<'a> {
    start: usize,
    end: usize,
    pattern_index: usize,
    path: &'a str,
    node_text: &'a str,
    match_kind: StructuredMatchKind,
}

impl<'a> From<&'a StructuredMatch<'_>> for StructuredMatchFields<'a> {
    fn from(matched: &'a StructuredMatch<'_>) -> Self {
        Self {
            start: matched.start,
            end: matched.end,
            pattern_index: matched.pattern_index,
            path: &matched.path,
            node_text: matched.node_text,
            match_kind: matched.match_kind,
        }
    }
}

impl<'a> From<&'a OwnedStructuredMatch> for StructuredMatchFields<'a> {
    fn from(matched: &'a OwnedStructuredMatch) -> Self {
        Self {
            start: matched.start,
            end: matched.end,
            pattern_index: matched.pattern_index,
            path: &matched.path,
            node_text: &matched.node_text,
            match_kind: matched.match_kind,
        }
    }
}

python_result! {
    PyStructuredMatch, "StructuredMatch",
    "A structured match. `start` and `end` are UTF-8 byte offsets of `node_text` within the \
     searched text or file.",
    from(
        py: Python<'_>,
        value: StructuredMatchFields<'_>,
        indexes: &[Py<PyInt>],
        mode: StructuredMode,
        context: NodeContext
    )
    fields {
        /// Byte offset where `node_text` starts.
        [get] start: Py<PyInt> = python_int(py, value.start);
        /// Byte offset just past the end of `node_text`.
        [get] end: Py<PyInt> = python_int(py, value.end);
        /// Position in `Searcher.patterns` of the pattern that matched.
        [get] pattern_index: Py<PyInt> = pattern_index(py, indexes, value.pattern_index);
        /// The jq-style path of the matched value, such as `$.events[1].status`.
        [get] path: Py<PyString> = PyString::new(py, value.path).unbind();
        /// The matched value exactly as the document writes it; `value()` reads it.
        [get] node_text: Py<PyString> = PyString::new(py, value.node_text).unbind();
        match_kind: StructuredMatchKind = value.match_kind;
        mode: StructuredMode = mode;
        context: NodeContext = context;
    }
    compare [start, end, pattern_index, path, node_text, match_kind]
    methods {
        /// How the pattern matched.
        #[getter]
        fn match_kind(&self) -> StructuredMatchKind {
            self.match_kind
        }

        /// Reads `node_text` into the Python value it means, as byteseek's predicates read
        /// it, parsing it again on each call.
        ///
        /// Raises `ValueError` when `node_text` doesn't read as a value on its own.
        fn value<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
            node_value(
                py,
                self.mode,
                self.node_text.bind(py).to_str()?,
                self.path.bind(py).to_str()?,
                &self.context,
            )
        }
    }
}

python_result! {
    PyStructuredFileMatchResult, "StructuredFileMatchResult",
    "The structured matches in one file. Offsets in `matches` are UTF-8 byte offsets into the \
     file.",
    from(
        py: Python<'_>,
        path: PathBuf,
        matches: Vec<(NodeContext, OwnedStructuredMatch)>,
        mode: StructuredMode
    )
    fields {
        /// The file's path, decoded as `os.fsdecode` does.
        [get] path: Py<PyString> = path_text(py, &path);
        matches: Vec<(NodeContext, OwnedStructuredMatch)> = matches;
        mode: StructuredMode = mode;
    }
    compare [path, matches]
    matches(self, py) PyTuple::new(
        py,
        self.matches.iter().map(|(context, matched)| {
            PyStructuredMatch::from_rust(py, matched.into(), &[], self.mode, context.clone_ref(py))
        }),
    );
}

/// Builds a tuple of `Match` objects without the searcher's shared pattern indexes.
fn match_tuple<'py>(
    py: Python<'py>,
    matches: impl IntoIterator<Item = Match, IntoIter: ExactSizeIterator>,
) -> PyResult<Bound<'py, PyTuple>> {
    PyTuple::new(
        py,
        matches
            .into_iter()
            .map(|matched| PyMatch::from_rust(py, matched, &[])),
    )
}

/// Converts a path to text as `os.fsdecode` does, so non-UTF-8 names still open the file.
fn path_text(py: Python<'_>, path: &Path) -> Py<PyString> {
    // The error type is uninhabited, so this pattern is irrefutable.
    let Ok(text) = path.as_os_str().into_pyobject(py);
    text.unbind()
}

fn python_int(py: Python<'_>, value: usize) -> Py<PyInt> {
    // CPython builds an `int` faster from a signed word, which every in-memory offset fits.
    match i64::try_from(value) {
        Ok(signed) => PyInt::new(py, signed),
        Err(_) => PyInt::new(py, value),
    }
    .unbind()
}

/// Returns the shared Python `int` for a pattern index, or a new one if it is not in
/// `indexes`.
fn pattern_index(py: Python<'_>, indexes: &[Py<PyInt>], index: usize) -> Py<PyInt> {
    indexes
        .get(index)
        .map_or_else(|| python_int(py, index), |shared| shared.clone_ref(py))
}

fn object_list<'py, V, T: IntoPyObject<'py>>(
    py: Python<'py>,
    values: Vec<V>,
    convert: impl Fn(V) -> T,
) -> PyResult<Bound<'py, PyList>> {
    PyList::new(py, values.into_iter().map(convert))
}

/// Search options. Every option is a keyword argument; see the constructor for each.
#[pyclass(
    frozen,
    eq,
    hash,
    skip_from_py_object,
    name = "SearchConfig",
    module = "byteseek"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PySearchConfig {
    inner: SearchConfig,
}

/// Passes a keyword argument through unchanged.
fn accept<T>(_name: &str, value: T) -> PyResult<T> {
    Ok(value)
}

/// Rejects a negative limit with a `ValueError` naming its argument.
fn non_negative(name: &str, value: Option<isize>) -> PyResult<Option<usize>> {
    value
        .map(|limit| {
            usize::try_from(limit).map_err(|_| {
                PyValueError::new_err(format!("{name} must be at least 0, not {limit}"))
            })
        })
        .transpose()
}

// PyO3 must see the complete impl after expansion; method-level macros are not supported.
macro_rules! python_config {
    ($( $(#[doc = $doc:literal])* $field:ident: $argument:ty = $default:expr => $validate:ident; )*) => {
        #[pymethods]
        impl PySearchConfig {
            /// Creates a search configuration.
            ///
            /// * `case_mode` - How letter case matches.
            /// * `boolean_queries` - Read each pattern as a query of terms joined with `AND`,
            ///   `OR`, and `NOT`, which selects whole lines.
            /// * `whole_word` - Match only where a pattern isn't joined to word characters.
            /// * `overlapping` - Report matches that overlap one another.
            /// * `ignore_gitignore` - Search files that `.gitignore` rules exclude.
            /// * `max_matches` - Stop after this many matches.
            /// * `max_edit_distance` - Also match text within this many insertions, deletions,
            ///   or substitutions of a pattern.
            /// * `structured_mode` - Parse texts and files as this format for structured search.
            /// * `structured_predicate_handling` - What predicates do with mistyped values.
            /// * `path_order` - The order directory searches return files in.
            ///
            /// Enum options also accept their values as strings, such as `"json"`. Raises
            /// `ValueError` for an unknown value or a negative limit.
            #[new]
            // Keyword-only, since ten positional options would not read.
            #[pyo3(signature = (*, $($field = $default),*))]
            #[allow(clippy::too_many_arguments)]
            fn new($($field: $argument),*) -> PyResult<Self> {
                Ok(Self {
                    inner: SearchConfig::builder()
                        $(.$field($validate(stringify!($field), $field)?))*
                        .build(),
                })
            }

            $(
                $(#[doc = $doc])*
                #[getter]
                fn $field<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
                    self.inner.$field().into_bound_py_any(py)
                }
            )*

            /// Lists the options that differ from their defaults.
            fn __repr__(&self) -> String {
                let defaults = SearchConfig::default();
                let options: Vec<String> = [
                    $(
                        (self.inner.$field() != defaults.$field()).then(|| {
                            format!("{}={}", stringify!($field), self.inner.$field().literal())
                        }),
                    )*
                ]
                .into_iter()
                .flatten()
                .collect();
                format!("SearchConfig({})", options.join(", "))
            }

            /// Pickles the options as the constructor's keyword arguments.
            fn __getnewargs_ex__<'py>(&self, py: Python<'py>)
                -> PyResult<(Bound<'py, PyTuple>, Bound<'py, PyDict>)>
            {
                let options = PyDict::new(py);
                $(options.set_item(intern!(py, stringify!($field)), self.inner.$field())?;)*
                Ok((PyTuple::empty(py), options))
            }
        }
    };
}

python_config! {
    /// How letter case matches.
    case_mode: CaseMode = CaseMode::Sensitive => accept;
    /// Whether each pattern is a boolean query that selects whole lines.
    boolean_queries: bool = false => accept;
    /// Whether matches must not be joined to word characters.
    whole_word: bool = false => accept;
    /// Whether matches may overlap one another.
    overlapping: bool = false => accept;
    /// Whether directory searches include files that `.gitignore` rules exclude.
    ignore_gitignore: bool = false => accept;
    /// The most matches a search returns, or `None` for no limit.
    max_matches: Option<isize> = None => non_negative;
    /// The edit distance fuzzy matching allows, or `None` for exact matching.
    max_edit_distance: Option<isize> = None => non_negative;
    /// The format structured searches parse, or `None` when they are off.
    structured_mode: Option<StructuredMode> = None => accept;
    /// What structured predicates do with values of the wrong type.
    structured_predicate_handling: StructuredPredicateHandling =
        StructuredPredicateHandling::Ignore => accept;
    /// The order directory searches return files in.
    path_order: PathOrder = PathOrder::Discovery => accept;
}

/// Compiled search patterns, reused for every search of strings, files, or directories.
///
/// Searches release the GIL, so threads can search at once. Search methods raise
/// `SearchError` for structured input that doesn't parse.
// Searchers are immutable after construction; `frozen` drops the per-call borrow checks.
#[pyclass(frozen, name = "Searcher", module = "byteseek")]
struct PySearcher {
    inner: Searcher,
    patterns: Py<PyTuple>,
    /// Each pattern's index as a Python `int`, shared by its matches.
    pattern_indexes: Vec<Py<PyInt>>,
}

/// Runs a search without the GIL, raising its error as a Python exception.
fn search<T: Send>(
    py: Python<'_>,
    run: impl FnOnce() -> Result<T, crate::SearchError> + Send,
) -> PyResult<T> {
    py.detach(run).map_err(to_python_error)
}

#[pymethods]
impl PySearcher {
    /// Compiles `patterns`, a string or an iterable of strings, with `config`.
    ///
    /// Raises `SearchError` when there are no patterns, a pattern is empty, or a pattern
    /// isn't valid for the configured mode.
    #[new]
    #[pyo3(signature = (patterns, config=None))]
    fn new(
        py: Python<'_>,
        patterns: &Bound<'_, PyAny>,
        config: Option<&PySearchConfig>,
    ) -> PyResult<Self> {
        let patterns = match patterns.cast::<PyString>() {
            Ok(pattern) => vec![pattern.clone()],
            Err(_) => patterns
                .try_iter()?
                .map(|pattern| Ok(pattern?.cast_into::<PyString>()?))
                .collect::<PyResult<Vec<_>>>()?,
        };
        let texts = patterns
            .iter()
            .map(|pattern| pattern.to_str())
            .collect::<PyResult<Vec<&str>>>()?;
        let search_config = config.map_or(SearchConfig::default(), |value| value.inner);
        // Automaton construction can be slow for many patterns; let other threads run.
        let inner = search(py, || Searcher::new(texts.iter().copied(), search_config))?;
        let pattern_indexes = (0..inner.pattern_count())
            .map(|index| python_int(py, index))
            .collect();
        Ok(Self {
            inner,
            patterns: PyTuple::new(py, patterns)?.unbind(),
            pattern_indexes,
        })
    }

    /// The patterns as a tuple, in the order `pattern_index` counts them.
    #[getter]
    fn patterns(&self, py: Python<'_>) -> Py<PyTuple> {
        self.patterns.clone_ref(py)
    }

    /// The options the patterns were compiled with.
    #[getter]
    fn config(&self) -> PySearchConfig {
        PySearchConfig {
            inner: self.inner.config(),
        }
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let patterns = PyList::new(py, self.patterns.bind(py))?.repr()?;
        let config = self.inner.config();
        Ok(if config == SearchConfig::default() {
            format!("Searcher({patterns})")
        } else {
            format!(
                "Searcher({patterns}, {})",
                PySearchConfig { inner: config }.__repr__()
            )
        })
    }

    /// Pickles the searcher as its patterns and options; unpickling compiles them again.
    fn __getnewargs__(&self, py: Python<'_>) -> (Py<PyTuple>, PySearchConfig) {
        (self.patterns(py), self.config())
    }

    /// Returns every match in `text`, in order.
    fn find<'py>(&self, py: Python<'py>, text: &str) -> PyResult<Bound<'py, PyList>> {
        let matches = search(py, || self.inner.try_find(text))?;
        object_list(py, matches, |matched| {
            PyMatch::from_rust(py, matched, &self.pattern_indexes)
        })
    }

    /// Returns the first match in `text`, or `None`.
    fn find_first(&self, py: Python<'_>, text: &str) -> PyResult<Option<PyMatch>> {
        let matched = search(py, || self.inner.try_find_first(text))?;
        Ok(matched.map(|matched| PyMatch::from_rust(py, matched, &self.pattern_indexes)))
    }

    /// Returns whether any pattern matches in `text`.
    fn contains(&self, py: Python<'_>, text: &str) -> PyResult<bool> {
        search(py, || self.inner.try_contains(text))
    }

    /// Returns the number of matches in `text`.
    fn count(&self, py: Python<'_>, text: &str) -> PyResult<usize> {
        search(py, || self.inner.try_count(text))
    }

    /// Returns each line of `text` that holds a match, with its matches.
    ///
    /// With `parallel`, large texts are searched on several threads; the lines are the
    /// same either way.
    #[pyo3(signature = (text, *, parallel=false))]
    fn find_lines<'py>(
        &self,
        py: Python<'py>,
        text: &str,
        parallel: bool,
    ) -> PyResult<Bound<'py, PyList>> {
        let lines = py.detach(|| {
            if parallel {
                self.inner.find_lines_parallel(text)
            } else {
                self.inner.find_lines(text)
            }
        });
        object_list(py, lines, |line| PyLineMatch::from_rust(py, line))
    }

    /// Searches a file, or every file under a directory, skipping hidden and binary files
    /// and files `.gitignore` excludes. Returns one result per file with a match.
    ///
    /// Files under a directory that are not valid UTF-8 are passed over; a path naming such
    /// a file raises `SearchError`. Raises `FileNotFoundError` when `path` doesn't exist.
    // `PathBuf` accepts `str` and `os.PathLike` arguments alike.
    fn find_path<'py>(&self, py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyList>> {
        let results = search(py, || self.inner.find_path(&path))?;
        object_list(py, results, |result| {
            PyFileMatchResult::from_rust(py, result)
        })
    }

    /// Parses `text` in the configured structured format and returns every match.
    ///
    /// Raises `SearchError` when structured mode is off or `text` doesn't parse.
    fn find_structured<'py>(
        &self,
        py: Python<'py>,
        text: &Bound<'py, PyString>,
    ) -> PyResult<Bound<'py, PyList>> {
        let mode = self.structured_mode()?;
        let source = text.to_str()?;
        let matches = search(py, || self.inner.find_structured(source))?;
        object_list(py, matches, |matched| {
            let context = NodeContext::of(mode, source, matched.start, matched.node_text, || {
                TomlSource::Text(text.clone().unbind())
            });
            let fields = StructuredMatchFields::from(&matched);
            PyStructuredMatch::from_rust(py, fields, &self.pattern_indexes, mode, context)
        })
    }

    /// Parses a file, or every file under a directory, in the configured structured format
    /// and returns one result per file with a match.
    ///
    /// Files under a directory that are not valid UTF-8 are passed over; a path naming such
    /// a file raises `SearchError`. Raises `FileNotFoundError` when `path` doesn't exist.
    // `PathBuf` accepts `str` and `os.PathLike` arguments alike.
    fn find_structured_path<'py>(
        &self,
        py: Python<'py>,
        path: PathBuf,
    ) -> PyResult<Bound<'py, PyList>> {
        let mode = self.structured_mode()?;
        let results = search(py, || {
            self.inner
                .find_structured_path_with(&path, |matches, contents| {
                    let mut shared: Option<Arc<str>> = None;
                    matches
                        .into_iter()
                        .map(|matched| {
                            let context = NodeContext::of(
                                mode,
                                contents,
                                matched.start,
                                matched.node_text,
                                || {
                                    let document = shared.get_or_insert_with(|| contents.into());
                                    TomlSource::File(Arc::clone(document))
                                },
                            );
                            (context, OwnedStructuredMatch::from(matched))
                        })
                        .collect()
                })
        })?;
        object_list(py, results, |(path, matches)| {
            PyStructuredFileMatchResult::from_rust(py, path, matches, mode)
        })
    }
}

impl PySearcher {
    /// The format structured searches parse, or the error they raise without one.
    fn structured_mode(&self) -> PyResult<StructuredMode> {
        self.inner
            .config()
            .structured_mode()
            .ok_or_else(|| to_python_error(crate::SearchError::StructuredModeDisabled))
    }
}

/// Raises `FileNotFoundError` for a path that doesn't exist, as `open()` does, and
/// `SearchError` for everything else.
fn to_python_error(error: crate::SearchError) -> PyErr {
    match &error {
        crate::SearchError::FileTraversal { path, .. }
        | crate::SearchError::FileRead { path, .. }
            if !Path::new(path).exists() =>
        {
            PyFileNotFoundError::new_err((ENOENT, "No such file or directory", path.clone()))
        }
        _ => SearchError::new_err(error.to_string()),
    }
}

/// Runs the `byteseek` command line on `args`, the arguments after the program name, without
/// holding the GIL, and returns its exit status.
#[pyfunction]
fn run_cli(py: Python<'_>, args: Vec<OsString>) -> u8 {
    py.detach(|| crate::cli::run(iter::once(OsString::from("byteseek")).chain(args)))
}

#[pymodule(name = "_byteseek")]
fn byteseek(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("SearchError", module.py().get_type::<SearchError>())?;
    module.add_class::<PyMatch>()?;
    module.add_class::<PyLineMatch>()?;
    module.add_class::<PyFileMatchResult>()?;
    module.add_class::<PyStructuredMatch>()?;
    module.add_class::<PyStructuredFileMatchResult>()?;
    module.add_class::<PySearchConfig>()?;
    module.add_class::<PySearcher>()?;
    module.add_function(wrap_pyfunction!(run_cli, module)?)?;
    Ok(())
}
