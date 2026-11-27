"""Structured and directory workloads, each with byteseek and plain-Python implementations.

Every Python implementation is what someone would write without byteseek: parse the whole
document with a standard parser, then index into it when the layout is known, or walk it
when it isn't. Both sides return the same `(path, value)` rows, so the comparison checks
that they agree before timing them.
"""

from __future__ import annotations

import json
import os
import re
import tomllib
from collections.abc import Callable, Iterable
from concurrent.futures import Executor
from pathlib import Path
from typing import Any, TypeVar

import orjson
import rtoml
import yaml

from byteseek import PathOrder, SearchConfig, Searcher, StructuredMode

# Parsers return documents of arbitrary shape, which the baselines index by known layout.
Loads = Callable[[str | bytes], Any]
TomlLoads = Callable[[str], Any]
YamlLoader = type[yaml.SafeLoader] | type[yaml.CSafeLoader]
Row = tuple[str, object]
Mapped = TypeVar("Mapped")
# A file's path relative to the searched directory, with its matches.
FileStarts = tuple[str, list[int]]
FileRows = tuple[str, list[Row]]

SERVICES = ("auth", "billing", "search", "checkout", "inventory")
IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_]*", re.ASCII)

# Python's documented YAML loaders: the pure-Python default and the libyaml-backed one.
YAML_LOADERS: dict[str, YamlLoader] = {
    "pyyaml SafeLoader": yaml.SafeLoader,
    "pyyaml CSafeLoader": yaml.CSafeLoader,
}
JSON_LOADERS: dict[str, Loads] = {"json": json.loads, "orjson": orjson.loads}
# The standard library's parser and a compiled one.
TOML_LOADERS: dict[str, TomlLoads] = {"tomllib": tomllib.loads, "rtoml": rtoml.loads}


def build_events_document(count: int, start: int = 0) -> str:
    """An API dump of `count` events, indented as most JSON files are."""
    events = []
    for index in range(start, start + count):
        events.append(
            {
                "id": index,
                "service": SERVICES[index % len(SERVICES)],
                "payload": {
                    "user": f"user{index % 997}",
                    "latency_ms": (index * 37) % 2000,
                    "status": "failed" if index % 13 == 0 else "ok",
                    "error": "upstream timeout" if index % 101 == 0 else None,
                },
                "tags": ["api", "v2"],
            }
        )
    return json.dumps({"generated": "2026-01-01T00:00:00Z", "events": events}, indent=2)


def build_log_records(count: int) -> str:
    """JSON Lines logs, one compact record per line."""
    return "".join(
        json.dumps(
            {
                "ts": f"2026-01-01T00:{index % 60:02d}:{(index * 7) % 60:02d}Z",
                "level": ("INFO", "WARN", "ERROR")[index % 3],
                "service": SERVICES[index % len(SERVICES)],
                "latency_ms": (index * 37) % 2000,
                "route": f"/api/v2/items/{index % 500}",
            },
            separators=(",", ":"),
        )
        + "\n"
        for index in range(count)
    )


def build_manifests(count: int) -> str:
    """A multi-document stream of Kubernetes-style deployments."""
    return "---\n".join(
        f"""apiVersion: apps/v1
kind: Deployment
metadata:
  name: svc-{index}
  namespace: team-{index % 7}
spec:
  replicas: {index % 6}
  template:
    spec:
      containers:
        - name: app
          image: registry.example.com/svc-{index}:1.{index % 20}.0
          resources:
            limits:
              memory: {128 * (1 + index % 4)}Mi
"""
        for index in range(count)
    )


def build_lockfile(count: int) -> str:
    """A Cargo.lock-style TOML file with `count` packages."""
    return "".join(
        f"""[[package]]
name = "crate-{index}"
version = "0.{index % 50}.{index % 7}"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{(index * 0x9E3779B97F4A7C15) % (1 << 128):032x}"
dependencies = ["crate-{(index * 7) % count}", "crate-{(index * 13) % count}"]

"""
        for index in range(count)
    )


def write_tree(root: Path, files: int, contents: Callable[[int], str], suffix: str) -> None:
    """Writes `files` files spread over ten subdirectories of `root`."""
    for index in range(files):
        directory = root / f"part{index % 10}"
        directory.mkdir(parents=True, exist_ok=True)
        (directory / f"file{index:04d}{suffix}").write_text(contents(index), encoding="utf-8")


def path_key(key: str) -> str:
    """Formats one object key the way byteseek prints paths."""
    if IDENTIFIER.fullmatch(key):
        return f".{key}"
    return f"[{json.dumps(key, ensure_ascii=False)}]"


def strings_containing(node: object, needle: str, path: str = "$") -> list[Row]:
    """Finds string scalars containing `needle`, as a byteseek text pattern does, in document
    order. Paths are only formatted for matches."""
    found: list[Row] = []
    segments: list[str | int] = []

    def visit(current: object) -> None:
        if isinstance(current, dict):
            for key, child in current.items():
                segments.append(key)
                visit(child)
                segments.pop()
        elif isinstance(current, list):
            for index, child in enumerate(current):
                segments.append(index)
                visit(child)
                segments.pop()
        elif isinstance(current, str) and needle in current:
            parts = (f"[{part}]" if isinstance(part, int) else path_key(part) for part in segments)
            found.append((path + "".join(parts), current))

    visit(node)
    return found


def searched(searcher: Searcher, haystack: str) -> list[Row]:
    """Runs byteseek and reads each match as a Python value."""
    return [(match.path, match.value()) for match in searcher.find_structured(haystack)]


def structured(patterns: list[str], mode: StructuredMode) -> Searcher:
    return Searcher(patterns, SearchConfig(structured_mode=mode))


def json_lookup(loads: Loads, haystack: str, index: int) -> list[Row]:
    events = loads(haystack)["events"]
    return [(f"$.events[{index}].payload.user", events[index]["payload"]["user"])]


def json_slow_failures(loads: Loads, haystack: str | bytes) -> list[Row]:
    events = loads(haystack)["events"]
    return [
        (f"$.events[{index}].payload.latency_ms", payload["latency_ms"])
        for index, event in enumerate(events)
        if (payload := event["payload"])["latency_ms"] > 1500 and payload["status"] == "failed"
    ]


def json_text_search(loads: Loads, haystack: str, needle: str) -> list[Row]:
    return strings_containing(loads(haystack), needle)


def jsonl_slow_records(loads: Loads, haystack: str) -> list[Row]:
    return [
        ("$.latency_ms", record["latency_ms"])
        for line in haystack.splitlines()
        if (record := loads(line))["latency_ms"] > 1900
    ]


def yaml_scaled_up(loader: YamlLoader, haystack: str) -> list[Row]:
    return [
        (f"$[{index}].spec.replicas", document["spec"]["replicas"])
        for index, document in enumerate(yaml.load_all(haystack, Loader=loader))
        if document["spec"]["replicas"] >= 4
    ]


def yaml_text_search(loader: YamlLoader, haystack: str, needle: str) -> list[Row]:
    return [
        row
        for index, document in enumerate(yaml.load_all(haystack, Loader=loader))
        for row in strings_containing(document, needle, f"$[{index}]")
    ]


def toml_lookup(loads: TomlLoads, haystack: str, index: int) -> list[Row]:
    return [(f"$.package[{index}].version", loads(haystack)["package"][index]["version"])]


def toml_text_search(loads: TomlLoads, haystack: str, needle: str) -> list[Row]:
    return strings_containing(loads(haystack), needle)


def relative(root: Path, path: str) -> str:
    return os.path.relpath(path, root)


def tree_files(root: Path, suffix: str = "") -> list[str]:
    """Lists the files under `root` in path order, as `os.walk` users do."""
    return sorted(
        os.path.join(directory, name)
        for directory, _, names in os.walk(root)
        for name in names
        if name.endswith(suffix)
    )


def byteseek_text_tree(searcher: Searcher, root: Path) -> list[FileStarts]:
    return [
        (relative(root, result.path), [match.start for match in result.matches])
        for result in searcher.find_path(root)
    ]


def byteseek_structured_tree(searcher: Searcher, root: Path) -> list[FileRows]:
    return [
        (relative(root, result.path), [(match.path, match.value()) for match in result.matches])
        for result in searcher.find_structured_path(root)
    ]


def path_ordered(patterns: list[str], mode: StructuredMode | None = None) -> Searcher:
    return Searcher(patterns, SearchConfig(structured_mode=mode, path_order=PathOrder.PATH))


def map_files(
    read: Callable[[str], Mapped], files: list[str], executor: Executor | None
) -> Iterable[Mapped]:
    """Applies `read` to each file on `executor`, or on this thread without one, as a
    single-threaded program would, with no hand-off per file."""
    return map(read, files) if executor is None else executor.map(read, files)


def python_text_tree(
    find_starts: Callable[[bytes], Iterable[int]], root: Path, executor: Executor | None
) -> list[FileStarts]:
    """Reads every file under `root`, with `map_files`, and finds the pattern's byte
    offsets, as byteseek reports them, in the file's bytes."""
    files = tree_files(root)
    starts = map_files(lambda path: list(find_starts(Path(path).read_bytes())), files, executor)
    return [(relative(root, path), found) for path, found in zip(files, starts) if found]


def python_structured_tree(
    loads: Loads, root: Path, executor: Executor | None
) -> list[FileRows]:
    """Loads every JSON file under `root`, with `map_files`, and filters its events."""
    files = tree_files(root, ".json")
    rows = map_files(
        lambda path: json_slow_failures(loads, Path(path).read_bytes()), files, executor
    )
    return [(relative(root, path), found) for path, found in zip(files, rows) if found]
