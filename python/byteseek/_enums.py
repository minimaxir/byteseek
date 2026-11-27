"""Search options and match kinds. Each member is also its lowercase name as a string, so
`SearchConfig(structured_mode="json")` works as well as `StructuredMode.JSON`."""

from enum import StrEnum, auto


class CaseMode(StrEnum):
    """How patterns match letter case."""

    SENSITIVE = auto()
    """Letters match only in the case the pattern spells them."""
    ASCII_INSENSITIVE = auto()
    """ASCII letters match in either case; other characters match exactly."""


class PathOrder(StrEnum):
    """The order in which directory searches return files."""

    DISCOVERY = auto()
    """The order the directory walk finds files in, which costs nothing extra."""
    PATH = auto()
    """Sorted by path."""


class StructuredMode(StrEnum):
    """The document format structured searches parse."""

    JSON = auto()
    YAML = auto()
    TOML = auto()


class StructuredPredicateHandling(StrEnum):
    """What a structured predicate does with a value of the wrong type, such as a number
    written as a string."""

    IGNORE = auto()
    """Skip the value."""
    ERROR = auto()
    """Raise `SearchError`."""


class StructuredMatchKind(StrEnum):
    """How a structured pattern matched."""

    PATH = auto()
    """A `$`-path pattern named the value's path."""
    VALUE = auto()
    """Plain text matched inside a scalar value."""
    PATH_VALUE = auto()
    """A `$.path=value` pattern matched both."""
    PREDICATE = auto()
    """A predicate such as `$.latency_ms > 500` held."""
