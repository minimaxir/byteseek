"""Fast literal, fuzzy, and structured (JSON, YAML, TOML) text search, written in Rust."""

from ._byteseek import (
    FileMatchResult,
    LineMatch,
    Match,
    SearchConfig,
    SearchError,
    Searcher,
    StructuredFileMatchResult,
    StructuredMatch,
)
from ._enums import (
    CaseMode,
    PathOrder,
    StructuredMatchKind,
    StructuredMode,
    StructuredPredicateHandling,
)

__all__ = [
    "CaseMode",
    "FileMatchResult",
    "LineMatch",
    "Match",
    "PathOrder",
    "SearchConfig",
    "SearchError",
    "Searcher",
    "StructuredFileMatchResult",
    "StructuredMatch",
    "StructuredMatchKind",
    "StructuredMode",
    "StructuredPredicateHandling",
]
