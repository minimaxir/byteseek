from __future__ import annotations

from collections.abc import Iterable
from datetime import date, datetime, time
from os import PathLike
from typing import Any, ClassVar, Literal, TypeAlias, final

from ._enums import CaseMode as CaseMode
from ._enums import PathOrder as PathOrder
from ._enums import StructuredMatchKind as StructuredMatchKind
from ._enums import StructuredMode as StructuredMode
from ._enums import StructuredPredicateHandling as StructuredPredicateHandling

# What `StructuredMatch.value()` returns. Only TOML has dates and times; JSON and YAML
# timestamps stay strings.
_StructuredValue: TypeAlias = (
    None
    | bool
    | int
    | float
    | str
    | datetime
    | date
    | time
    | list[_StructuredValue]
    | dict[str, _StructuredValue]
)

# Enum options also take their members' string values.
_CaseMode: TypeAlias = CaseMode | Literal["sensitive", "ascii_insensitive"]
_PathOrder: TypeAlias = PathOrder | Literal["discovery", "path"]
_StructuredMode: TypeAlias = StructuredMode | Literal["json", "yaml", "toml"]
_StructuredPredicateHandling: TypeAlias = StructuredPredicateHandling | Literal["ignore", "error"]

# Result objects are created by `Searcher` only. They are read-only, compare and hash by
# their fields, and support `match` statements by those fields in order.

@final
class Match:
    """One match. `start` and `end` are UTF-8 byte offsets into the searched text, not
    `str` indices: slice `text.encode()[match.start:match.end]` for non-ASCII text."""

    __match_args__ = ("start", "end", "pattern_index")
    @property
    def start(self) -> int:
        """Byte offset where the match starts."""
    @property
    def end(self) -> int:
        """Byte offset just past the match's end."""
    @property
    def pattern_index(self) -> int:
        """Position in `Searcher.patterns` of the pattern that matched."""
    def __hash__(self) -> int: ...

@final
class LineMatch:
    """A line holding at least one match. Offsets in `matches` are UTF-8 byte offsets
    into `line`."""

    __match_args__ = ("line_number", "line", "matches")
    @property
    def line_number(self) -> int:
        """The line's 1-based number."""
    @property
    def line(self) -> str:
        """The line without its line ending."""
    @property
    def matches(self) -> tuple[Match, ...]:
        """The matches, in the order they were found."""
    def __hash__(self) -> int: ...

@final
class FileMatchResult:
    """The matches in one file. Offsets in `matches` are UTF-8 byte offsets into the file."""

    __match_args__ = ("path", "matches")
    @property
    def path(self) -> str:
        """The file's path, decoded as `os.fsdecode` does."""
    @property
    def matches(self) -> tuple[Match, ...]:
        """The matches, in the order they were found."""
    def __hash__(self) -> int: ...

@final
class StructuredMatch:
    """A structured match. `start` and `end` are UTF-8 byte offsets of `node_text` within
    the searched text or file."""

    __match_args__ = ("start", "end", "pattern_index", "path", "node_text", "match_kind")
    @property
    def start(self) -> int:
        """Byte offset where `node_text` starts."""
    @property
    def end(self) -> int:
        """Byte offset just past the end of `node_text`."""
    @property
    def pattern_index(self) -> int:
        """Position in `Searcher.patterns` of the pattern that matched."""
    @property
    def path(self) -> str:
        """The jq-style path of the matched value, such as `$.events[1].status`."""
    @property
    def node_text(self) -> str:
        """The matched value exactly as the document writes it; `value()` reads it."""
    @property
    def match_kind(self) -> StructuredMatchKind:
        """How the pattern matched."""
    def __hash__(self) -> int: ...
    def value(self) -> _StructuredValue:
        """Reads `node_text` into the Python value it means, as byteseek's predicates
        read it, parsing it again on each call.

        Objects, mappings, and tables become `dict`s with `str` keys in document order,
        and arrays and sequences become `list`s. YAML plain scalars follow byteseek's
        reading: `yes` and `on` are `True`, `~` is `None`, `0755` is the integer 755, and
        `1:30` is 90. TOML datetimes become `datetime`, `date`, or `time`, truncated to
        microseconds. JSON integers beyond 64 bits become floats. A TOML table, or a value
        starting its line with `[`, is read from the searched text by its path, so TOML
        matches keep that text, or a copy of their file, alive.

        Raises `ValueError` when `node_text` doesn't read as a value: a YAML alias whose
        anchor lies outside it or whose expansion is too large, JSON nested more than 128
        levels deep, or a TOML date or time Python can't hold, such as a leap second."""

@final
class StructuredFileMatchResult:
    """The structured matches in one file. Offsets in `matches` are UTF-8 byte offsets
    into the file."""

    __match_args__ = ("path", "matches")
    @property
    def path(self) -> str:
        """The file's path, decoded as `os.fsdecode` does."""
    @property
    def matches(self) -> tuple[StructuredMatch, ...]:
        """The matches, in the order they were found."""
    def __hash__(self) -> int: ...

@final
class SearchConfig:
    """Search options. Configs are immutable, and compare and hash by their options."""

    def __init__(
        self,
        *,
        case_mode: _CaseMode = CaseMode.SENSITIVE,
        boolean_queries: bool = False,
        whole_word: bool = False,
        overlapping: bool = False,
        ignore_gitignore: bool = False,
        max_matches: int | None = None,
        max_edit_distance: int | None = None,
        structured_mode: _StructuredMode | None = None,
        structured_predicate_handling: _StructuredPredicateHandling = (
            StructuredPredicateHandling.IGNORE
        ),
        path_order: _PathOrder = PathOrder.DISCOVERY,
    ) -> None:
        """Creates a search configuration.

        * `case_mode` - How letter case matches.
        * `boolean_queries` - Read each pattern as a query of terms joined with `AND`,
          `OR`, and `NOT`, which selects whole lines.
        * `whole_word` - Match only where a pattern isn't joined to word characters.
        * `overlapping` - Report matches that overlap one another.
        * `ignore_gitignore` - Search files that `.gitignore` rules exclude.
        * `max_matches` - Stop after this many matches.
        * `max_edit_distance` - Also match text within this many insertions, deletions,
          or substitutions of a pattern.
        * `structured_mode` - Parse texts and files as this format for structured search.
        * `structured_predicate_handling` - What predicates do with mistyped values.
        * `path_order` - The order directory searches return files in.

        Raises `ValueError` for an unknown enum value or a negative limit."""
    @property
    def case_mode(self) -> CaseMode:
        """How letter case matches."""
    @property
    def boolean_queries(self) -> bool:
        """Whether each pattern is a boolean query that selects whole lines."""
    @property
    def whole_word(self) -> bool:
        """Whether matches must not be joined to word characters."""
    @property
    def overlapping(self) -> bool:
        """Whether matches may overlap one another."""
    @property
    def ignore_gitignore(self) -> bool:
        """Whether directory searches include files that `.gitignore` rules exclude."""
    @property
    def max_matches(self) -> int | None:
        """The most matches a search returns, or `None` for no limit."""
    @property
    def max_edit_distance(self) -> int | None:
        """The edit distance fuzzy matching allows, or `None` for exact matching."""
    @property
    def structured_mode(self) -> StructuredMode | None:
        """The format structured searches parse, or `None` when they are off."""
    @property
    def structured_predicate_handling(self) -> StructuredPredicateHandling:
        """What structured predicates do with values of the wrong type."""
    @property
    def path_order(self) -> PathOrder:
        """The order directory searches return files in."""
    def __hash__(self) -> int: ...
    def __getnewargs_ex__(self) -> tuple[tuple[()], dict[str, Any]]: ...

class SearchError(ValueError):
    """Raised when patterns or options are invalid, or a searched text or file can't be
    read."""

@final
class Searcher:
    """Compiled search patterns, reused for every search of strings, files, or directories.

    Searches release the GIL, so threads can search at once. Search methods raise
    `SearchError` for structured input that doesn't parse."""

    def __init__(self, patterns: str | Iterable[str], config: SearchConfig | None = None) -> None:
        """Compiles `patterns`, a string or an iterable of strings, with `config`.

        Raises `SearchError` when there are no patterns, a pattern is empty, or a pattern
        isn't valid for the configured mode."""
    @property
    def patterns(self) -> tuple[str, ...]:
        """The patterns as a tuple, in the order `pattern_index` counts them."""
    @property
    def config(self) -> SearchConfig:
        """The options the patterns were compiled with."""
    def __getnewargs__(self) -> tuple[tuple[str, ...], SearchConfig]: ...
    def find(self, text: str) -> list[Match]:
        """Returns every match in `text`, in order."""
    def find_first(self, text: str) -> Match | None:
        """Returns the first match in `text`, or `None`."""
    def contains(self, text: str) -> bool:
        """Returns whether any pattern matches in `text`."""
    def count(self, text: str) -> int:
        """Returns the number of matches in `text`."""
    def find_lines(self, text: str, *, parallel: bool = False) -> list[LineMatch]:
        """Returns each line of `text` that holds a match, with its matches.

        With `parallel`, large texts are searched on several threads; the lines are the
        same either way."""
    def find_path(self, path: str | PathLike[str]) -> list[FileMatchResult]:
        """Searches a file, or every file under a directory, skipping hidden and binary
        files and files `.gitignore` excludes. Returns one result per file with a match.

        Files under a directory that are not valid UTF-8 are passed over; a path naming such
        a file raises `SearchError`. Raises `FileNotFoundError` when `path` doesn't exist."""
    def find_structured(self, text: str) -> list[StructuredMatch]:
        """Parses `text` in the configured structured format and returns every match.

        Raises `SearchError` when structured mode is off or `text` doesn't parse."""
    def find_structured_path(self, path: str | PathLike[str]) -> list[StructuredFileMatchResult]:
        """Parses a file, or every file under a directory, in the configured structured
        format and returns one result per file with a match.

        Files under a directory that are not valid UTF-8 are passed over; a path naming such
        a file raises `SearchError`. Raises `FileNotFoundError` when `path` doesn't exist."""
