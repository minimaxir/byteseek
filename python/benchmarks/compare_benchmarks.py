from __future__ import annotations

import argparse
import concurrent.futures
import csv
import math
import os
import platform
import re
import statistics
import string
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from functools import partial
from importlib.metadata import version
from pathlib import Path
from typing import Any, Callable, Sequence

import ahocorasick  # type: ignore[import-not-found]
import orjson
import rtoml
import structured_workloads as workloads
import yaml

from byteseek import CaseMode, SearchConfig, Searcher, StructuredMode

MatchTuple = tuple[int, int, int]
LineTuple = tuple[int, str, list[MatchTuple]]
# Results are only compared for equality, so runners may return any comparable value.
Runner = Callable[[], object]

MASK_64: int = (1 << 64) - 1


def push_two_digits(value: int) -> str:
    return f"{value:02d}"


def build_log_corpus(lines: int) -> str:
    parts: list[str] = []
    parts_extend = parts.append

    for index in range(lines):
        level = ("INFO", "DEBUG", "WARN", "ERROR", "TRACE", "INFO", "WARN", "ERROR")[index % 8]
        minute = index % 60
        second = (index * 7) % 60
        status = "500" if index % 17 == 0 else "200"
        parts_extend(
            "2026-01-01T00:"
            f"{push_two_digits(minute)}:{push_two_digits(second)} "
            f"{level} service=auth request_id={index} status={status} "
            "message=handled user event\n"
        )

    return "".join(parts)


def build_source_corpus(repetitions: int) -> str:
    snippet = r"""
pub fn parse_record(input: &str) -> Result<Record, ParseError> {
    let mut fields = input.split(',');
    let id = fields.next().ok_or(ParseError::MissingField("id"))?;
    let category = fields.next().ok_or(ParseError::MissingField("category"))?;
    let payload = fields.next().ok_or(ParseError::MissingField("payload"))?;
    Ok(Record::new(id, category, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_record() {
        let result = parse_record("42,alpha,payload");
        assert!(result.is_ok());
    }
}
"""
    return snippet * repetitions


def build_dna_corpus(length: int) -> str:
    seed = 0x1234_5678_9ABC_DEF0
    data: list[str] = []
    data_append = data.append

    for _ in range(length):
        seed = ((seed * 6_364_136_223_846_793_005) + 1_442_695_040_888_963_407) & MASK_64
        next_value = (seed >> 32) & 0b11
        byte = ("A", "C", "G", "T")[next_value]
        data_append(byte)

    return "".join(data)


def build_overlap_corpus(repetitions: int) -> str:
    return "bananana" * repetitions


def normalize_rust_matches(searcher: Searcher, haystack: str) -> list[MatchTuple]:
    return [(match.start, match.end, match.pattern_index) for match in searcher.find(haystack)]


def normalize_rust_lines(searcher: Searcher, haystack: str, parallel: bool) -> list[LineTuple]:
    rows = searcher.find_lines(haystack, parallel=parallel)
    return [
        (
            line_match.line_number,
            line_match.line,
            [(match.start, match.end, match.pattern_index) for match in line_match.matches],
        )
        for line_match in rows
    ]


Finder = Callable[[str], list[MatchTuple]]

# Folds exactly the bytes byteseek's ASCII case-insensitive mode folds, keeping every offset.
ASCII_LOWERCASE = str.maketrans(string.ascii_uppercase, string.ascii_lowercase)


def ascii_fold(haystack: str, folds: bool) -> str:
    return haystack.translate(ASCII_LOWERCASE) if folds else haystack


@dataclass(frozen=True)
class RegexFinder:
    """Literal patterns with `re`. Case-insensitive searches fold the haystack first, which
    is several times faster than `re.IGNORECASE`."""

    regex: re.Pattern[str]
    pattern_count: int
    folds: bool
    overlapping: bool

    def find(self, haystack: str) -> list[MatchTuple]:
        matches = self.regex.finditer(ascii_fold(haystack, self.folds))
        if self.overlapping:
            return [(match.start(), match.end(1), 0) for match in matches]
        if self.pattern_count == 1:
            return [(match.start(), match.end(), 0) for match in matches]
        # Groups are named after pattern indexes, which `lastgroup` reports.
        return [(match.start(), match.end(), int(match.lastgroup[1:])) for match in matches]  # type: ignore[index]


def build_regex_finder(
    patterns: Sequence[str], case_mode: CaseMode, overlapping: bool
) -> RegexFinder:
    folds = case_mode == CaseMode.ASCII_INSENSITIVE
    literals = [ascii_fold(pattern, folds) for pattern in patterns]
    if overlapping:
        regex = re.compile(f"(?=({re.escape(literals[0])}))")
    elif len(literals) == 1:
        regex = re.compile(re.escape(literals[0]))
    else:
        # Longest first, so alternation matches leftmost-longest as byteseek does.
        ordered = sorted(range(len(literals)), key=lambda index: (-len(literals[index]), index))
        regex = re.compile(
            "|".join(f"(?P<P{index}>{re.escape(literals[index])})" for index in ordered)
        )
    return RegexFinder(regex, len(literals), folds, overlapping)


@dataclass(frozen=True)
class AhoCorasickFinder:
    """Literal patterns with pyahocorasick, a compiled Aho-Corasick automaton: leftmost-longest
    matches, or every match when overlapping."""

    automaton: Any
    folds: bool
    overlapping: bool

    def find(self, haystack: str) -> list[MatchTuple]:
        text = ascii_fold(haystack, self.folds)
        found = self.automaton.iter(text) if self.overlapping else self.automaton.iter_long(text)
        return [(end - length + 1, end + 1, index) for end, (index, length) in found]


def build_aho_corasick_finder(
    patterns: Sequence[str], case_mode: CaseMode, overlapping: bool
) -> AhoCorasickFinder:
    folds = case_mode == CaseMode.ASCII_INSENSITIVE
    automaton = ahocorasick.Automaton()
    for index, pattern in enumerate(patterns):
        automaton.add_word(ascii_fold(pattern, folds), (index, len(pattern)))
    automaton.make_automaton()
    return AhoCorasickFinder(automaton, folds, overlapping)


def find_lines(find: Finder, haystack: str, first_line: int = 1) -> list[LineTuple]:
    """Searches the whole haystack once, then groups matches by line, with offsets relative
    to their line. Patterns never contain newlines, so no match spans two lines."""
    rows: list[LineTuple] = []
    line_number, counted_to = first_line, 0
    line_start = line_end = -1
    current: list[MatchTuple] = []
    for start, end, index in find(haystack):
        if start >= line_end:
            line_number += haystack.count("\n", counted_to, start)
            counted_to = start
            line_start = haystack.rfind("\n", 0, start) + 1
            line_end = haystack.find("\n", start)
            if line_end == -1:
                line_end = len(haystack)
            current = []
            rows.append((line_number, haystack[line_start:line_end], current))
        current.append((start - line_start, end - line_start, index))
    return rows


def find_lines_parallel(
    find: Finder,
    haystack: str,
    executor: concurrent.futures.ThreadPoolExecutor,
    workers: int,
) -> list[LineTuple]:
    """Splits the haystack at line ends into one chunk per worker and searches each."""
    chunk_count = max(2, min(workers, 8))
    bounds = [0]
    for chunk in range(1, chunk_count):
        cut = haystack.find("\n", max(bounds[-1], len(haystack) * chunk // chunk_count))
        if cut == -1:
            break
        bounds.append(cut + 1)
    bounds.append(len(haystack))
    first_lines = [1 + haystack.count("\n", 0, start) for start in bounds[:-1]]
    chunks = [haystack[start:end] for start, end in zip(bounds, bounds[1:])]
    results = executor.map(partial(find_lines, find), chunks, first_lines)
    return [row for rows in results for row in rows]


@dataclass(frozen=True)
class BenchmarkCase:
    name: str
    rust_runner: Runner
    baselines: tuple[tuple[str, Runner], ...]


@dataclass(frozen=True)
class BenchConfig:
    warmups: int
    iterations: int


def timed_run(runner: Runner) -> tuple[object, int]:
    start = time.perf_counter_ns()
    result = runner()
    return result, time.perf_counter_ns() - start


# The confidence level of the interval reported around each median time.
CONFIDENCE_LEVEL = 0.95


@dataclass(frozen=True)
class Timing:
    """A runner's median time and the confidence interval around it, in nanoseconds."""

    median_ns: int
    lower_ns: int
    upper_ns: int


def median_interval(samples: Sequence[int]) -> tuple[int, int]:
    """Returns the bounds of the confidence interval of the median of `samples`, which assumes
    nothing of their distribution: the two samples equally far from the lowest and the highest
    that have the median between them with a probability of at least `CONFIDENCE_LEVEL`, or
    the lowest and the highest when there are too few samples for that."""
    ordered = sorted(samples)
    count = len(ordered)
    tail = (1 - CONFIDENCE_LEVEL) / 2
    # How many samples lie below the median is binomial with even odds.
    skipped = 0
    probability = 0.0
    for below in range(count):
        probability += math.comb(count, below) / 2**count
        if probability > tail:
            break
        skipped = below + 1
    skipped = max(skipped, 1)
    return ordered[skipped - 1], ordered[count - skipped]


def benchmark_case(case: BenchmarkCase, config: BenchConfig) -> list[Timing]:
    """Returns the timing of byteseek, then of each baseline. Every sample rotates which
    runner goes first, and every output must match byteseek's."""
    runners = [case.rust_runner, *(runner for _, runner in case.baselines)]
    expected = case.rust_runner()
    samples: list[list[int]] = [[] for _ in runners]
    for iteration in range(config.warmups + config.iterations):
        shift = iteration % len(runners)
        for position in [*range(shift, len(runners)), *range(shift)]:
            result, elapsed_ns = timed_run(runners[position])
            if result != expected:
                raise RuntimeError(f"unstable output for case: {case.name}")
            if iteration >= config.warmups:
                samples[position].append(elapsed_ns)
    return [Timing(int(statistics.median(times)), *median_interval(times)) for times in samples]


def validate_equivalence(cases: Sequence[BenchmarkCase]) -> None:
    mismatches = [
        f"{case.name} ({name})"
        for case in cases
        for name, runner in case.baselines
        if runner() != case.rust_runner()
    ]
    if mismatches:
        joined = ", ".join(mismatches)
        raise RuntimeError(f"semantic mismatch for cases: {joined}")


def literal_baselines(
    haystack: str, patterns: Sequence[str], case_mode: CaseMode, overlapping: bool = False
) -> tuple[tuple[str, Runner], ...]:
    """`re`, plus pyahocorasick when several patterns or overlapping matches call for an
    automaton rather than a single literal search."""
    regex = build_regex_finder(patterns, case_mode, overlapping)
    baselines: list[tuple[str, Runner]] = [("re", partial(regex.find, haystack))]
    if len(patterns) > 1 or overlapping:
        automaton = build_aho_corasick_finder(patterns, case_mode, overlapping)
        baselines.append(("pyahocorasick", partial(automaton.find, haystack)))
    return tuple(baselines)


def build_cases(
    parallel_workers: int,
    executor: concurrent.futures.ThreadPoolExecutor,
) -> list[BenchmarkCase]:
    cases: list[BenchmarkCase] = []

    def add(
        name: str,
        searcher: Searcher,
        haystack: str,
        patterns: Sequence[str],
        case_mode: CaseMode = CaseMode.SENSITIVE,
        overlapping: bool = False,
    ) -> None:
        cases.append(
            BenchmarkCase(
                name=name,
                rust_runner=partial(normalize_rust_matches, searcher, haystack),
                baselines=literal_baselines(haystack, patterns, case_mode, overlapping),
            )
        )

    insensitive = SearchConfig(case_mode=CaseMode.ASCII_INSENSITIVE)
    for lines in (2_000, 20_000):
        logs = build_log_corpus(lines)
        add(f"single_pattern/sensitive_logs/{lines}", Searcher(["ERROR"]), logs, ["ERROR"])
        add(
            f"single_pattern/ascii_insensitive_logs/{lines}",
            Searcher(["warn"], insensitive),
            logs,
            ["warn"],
            CaseMode.ASCII_INSENSITIVE,
        )

    source_patterns = (
        "parse_record",
        "MissingField",
        "result.is_ok",
        "Record::new",
        "payload",
        "assert",
    )
    add(
        "multi_pattern/source_code_tokens",
        Searcher(list(source_patterns)),
        build_source_corpus(2_000),
        source_patterns,
    )

    dna_patterns = ("ACGT", "GTAC", "TTTTT", "CCCC", "GGGG", "AAAA")
    for length in (250_000, 1_000_000):
        dna = build_dna_corpus(length)
        add(f"dna_like/single_pattern/{length}", Searcher(["ACGTAC"]), dna, ["ACGTAC"])
        add(f"dna_like/multi_pattern/{length}", Searcher(list(dna_patterns)), dna, dna_patterns)

    add(
        "overlap_and_lines/overlap_heavy",
        Searcher(["ana"], SearchConfig(overlapping=True)),
        build_overlap_corpus(100_000),
        ["ana"],
        overlapping=True,
    )

    line_text = build_log_corpus(120_000)
    line_patterns = ("error", "warn", "status=500")
    rust_lines = Searcher(list(line_patterns), insensitive)
    finders: list[tuple[str, Finder]] = [
        ("re", build_regex_finder(line_patterns, CaseMode.ASCII_INSENSITIVE, False).find),
        (
            "pyahocorasick",
            build_aho_corasick_finder(line_patterns, CaseMode.ASCII_INSENSITIVE, False).find,
        ),
    ]
    cases.append(
        BenchmarkCase(
            name="overlap_and_lines/line_auto_dispatch",
            rust_runner=partial(normalize_rust_lines, rust_lines, line_text, False),
            baselines=tuple((name, partial(find_lines, find, line_text)) for name, find in finders),
        )
    )
    cases.append(
        BenchmarkCase(
            name=f"overlap_and_lines/line_parallel_workflow/{parallel_workers}_workers",
            rust_runner=partial(normalize_rust_lines, rust_lines, line_text, True),
            baselines=tuple(
                (
                    f"{name}, thread pool",
                    partial(find_lines_parallel, find, line_text, executor, parallel_workers),
                )
                for name, find in finders
            ),
        )
    )

    return cases


def build_structured_cases(
    executor: concurrent.futures.ThreadPoolExecutor | None, root: Path
) -> list[BenchmarkCase]:
    """Structured and directory searches against parsing everything in Python. Every byteseek
    runner reads its matches with `value()`, so both sides hand back Python values. Python
    reads directories on `executor`, or on the calling thread without one."""
    json_baselines = workloads.JSON_LOADERS.items()
    yaml_baselines = workloads.YAML_LOADERS.items()
    slow_failures = '$.payload.latency_ms > 1500 AND $.payload.status = "failed"'
    cases: list[BenchmarkCase] = []

    def add(name: str, rust: Runner, *baselines: tuple[str, Runner]) -> None:
        cases.append(BenchmarkCase(name=name, rust_runner=rust, baselines=baselines))

    def searched(patterns: list[str], mode: StructuredMode, haystack: str) -> Runner:
        return partial(workloads.searched, workloads.structured(patterns, mode), haystack)

    events = workloads.build_events_document(20_000)
    add(
        "json/path_lookup/20000_events",
        searched(["$.events[15000].payload.user"], StructuredMode.JSON, events),
        *(
            (name, partial(workloads.json_lookup, loads, events, 15_000))
            for name, loads in json_baselines
        ),
    )
    add(
        "json/predicate/20000_events",
        searched([slow_failures], StructuredMode.JSON, events),
        *(
            (name, partial(workloads.json_slow_failures, loads, events))
            for name, loads in json_baselines
        ),
    )
    add(
        "json/text_search/20000_events",
        searched(["timeout"], StructuredMode.JSON, events),
        *(
            (name, partial(workloads.json_text_search, loads, events, "timeout"))
            for name, loads in json_baselines
        ),
    )
    # Every event's user matches, so byteseek builds 20,000 match objects.
    add(
        "json/text_search_every_event/20000_events",
        searched(["user"], StructuredMode.JSON, events),
        *(
            (name, partial(workloads.json_text_search, loads, events, "user"))
            for name, loads in json_baselines
        ),
    )

    records = workloads.build_log_records(100_000)
    add(
        "jsonl/predicate/100000_records",
        searched(["$.latency_ms > 1900"], StructuredMode.JSON, records),
        *(
            (name, partial(workloads.jsonl_slow_records, loads, records))
            for name, loads in json_baselines
        ),
    )

    manifests = workloads.build_manifests(2_000)
    add(
        "yaml/predicate/2000_documents",
        searched(["$.spec.replicas >= 4"], StructuredMode.YAML, manifests),
        *(
            (name, partial(workloads.yaml_scaled_up, loader, manifests))
            for name, loader in yaml_baselines
        ),
    )
    add(
        "yaml/text_search/2000_documents",
        searched([":1.19.0"], StructuredMode.YAML, manifests),
        *(
            (name, partial(workloads.yaml_text_search, loader, manifests, ":1.19.0"))
            for name, loader in yaml_baselines
        ),
    )

    lockfile = workloads.build_lockfile(3_000)
    add(
        "toml/path_lookup/3000_packages",
        searched(["$.package[2500].version"], StructuredMode.TOML, lockfile),
        *(
            (name, partial(workloads.toml_lookup, loads, lockfile, 2_500))
            for name, loads in workloads.TOML_LOADERS.items()
        ),
    )
    add(
        "toml/text_search/3000_packages",
        searched(["crate-42"], StructuredMode.TOML, lockfile),
        *(
            (name, partial(workloads.toml_text_search, loads, lockfile, "crate-42"))
            for name, loads in workloads.TOML_LOADERS.items()
        ),
    )

    logs = root / "logs"
    log_file = build_log_corpus(500)
    workloads.write_tree(logs, 200, lambda _: log_file, ".log")
    status = re.compile(rb"status=500")
    add(
        "directory/text_search/200_files",
        partial(workloads.byteseek_text_tree, workloads.path_ordered(["status=500"]), logs),
        (
            "os.walk + re",
            partial(
                workloads.python_text_tree,
                lambda data: (match.start() for match in status.finditer(data)),
                logs,
                executor,
            ),
        ),
    )

    event_files = root / "events"
    workloads.write_tree(
        event_files, 200, lambda index: workloads.build_events_document(200, index * 200), ".json"
    )
    add(
        "directory/json_predicate/200_files",
        partial(
            workloads.byteseek_structured_tree,
            workloads.path_ordered([slow_failures], StructuredMode.JSON),
            event_files,
        ),
        *(
            (
                f"os.walk + {name}",
                partial(workloads.python_structured_tree, loads, event_files, executor),
            )
            for name, loads in json_baselines
        ),
    )
    return cases


@dataclass(frozen=True)
class ResultRow:
    case: str
    baseline: str
    byteseek: Timing
    python: Timing


# Third-party baselines, named by their distribution; any other baseline is the standard library.
BASELINE_PACKAGES = ("orjson", "pyyaml", "rtoml", "pyahocorasick")


def baseline_version(baseline: str) -> str:
    """The package and version a baseline ran, or the Python version for the standard library."""
    for package in BASELINE_PACKAGES:
        if package in baseline:
            return f"{package} {version(package)}"
    return f"python {platform.python_version()}"


def write_csv(path: Path, rows: Sequence[ResultRow], workers: int) -> None:
    with path.open("w", newline="", encoding="utf-8") as file:
        writer = csv.writer(file, lineterminator="\n")
        writer.writerow(
            (
                "case",
                "python_baseline",
                "baseline_version",
                "workers",
                "byteseek_ms",
                "python_ms",
                "speedup_byteseek_over_python",
                "byteseek_ms_ci_lower",
                "byteseek_ms_ci_upper",
                "python_ms_ci_lower",
                "python_ms_ci_upper",
                "confidence_level",
            )
        )
        for row in rows:
            writer.writerow(
                (
                    row.case,
                    row.baseline,
                    baseline_version(row.baseline),
                    workers,
                    f"{row.byteseek.median_ns / 1e6:.5f}",
                    f"{row.python.median_ns / 1e6:.5f}",
                    f"{row.python.median_ns / row.byteseek.median_ns:.2f}",
                    f"{row.byteseek.lower_ns / 1e6:.5f}",
                    f"{row.byteseek.upper_ns / 1e6:.5f}",
                    f"{row.python.lower_ns / 1e6:.5f}",
                    f"{row.python.upper_ns / 1e6:.5f}",
                    CONFIDENCE_LEVEL,
                )
            )


def run_benchmarks(cases: Sequence[BenchmarkCase], config: BenchConfig) -> list[ResultRow]:
    print("Benchmark results (median over iterations)")
    print("-" * 130)
    print(
        f"{'case':52} {'python baseline':24} {'byteseek(ms)':>14} "
        f"{'python(ms)':>14} {'python/byteseek':>16}"
    )
    print("-" * 130)

    rows: list[ResultRow] = []
    for case in cases:
        rust, *baselines = benchmark_case(case, config)
        for (name, _), python in zip(case.baselines, baselines):
            rows.append(ResultRow(case.name, name, rust, python))
            rust_ns, py_ns = rust.median_ns, python.median_ns
            ratio = py_ns / rust_ns if rust_ns > 0 else float("inf")
            print(
                f"{case.name:52} {name:24} {rust_ns / 1e6:14.3f} {py_ns / 1e6:14.3f} {ratio:15.2f}x"
            )

    print("-" * 130)
    print(
        "Scope: in-process API workflows on deterministic synthetic corpora; per-case ratios only."
    )
    return rows


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Compare byteseek vs canonical Python benchmarks.")
    parser.add_argument("--warmups", type=int, default=5, help="Warmup rounds per case.")
    parser.add_argument("--iterations", type=int, default=31, help="Measured rounds per case.")
    parser.add_argument("--filter", default="", help="Run only cases whose name contains this.")
    parser.add_argument("--csv", type=Path, help="Also write the results to this CSV file.")
    parser.add_argument(
        "--parallel-workers",
        type=int,
        default=1,
        help="Equal Rayon and Python worker count; use more than one only for workflow scaling.",
    )
    return parser.parse_args()


def git_revision() -> str:
    completed = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        check=True,
        cwd=os.path.dirname(os.path.dirname(os.path.dirname(__file__))),
        stdout=subprocess.PIPE,
        text=True,
    )
    return completed.stdout.strip()


def print_metadata(parallel_workers: int) -> None:
    print("Benchmark metadata")
    print(f"revision: {git_revision()}")
    print(f"python: {sys.version.split()[0]}")
    print(f"platform: {platform.platform()}")
    print(f"processor: {platform.processor() or 'unknown'}")
    print(f"logical CPUs: {os.cpu_count() or 'unknown'}")
    print(
        f"orjson: {orjson.__version__}, pyyaml: {yaml.__version__}, "
        f"rtoml: {rtoml.__version__}, pyahocorasick: {version('pyahocorasick')}"
    )
    print(f"Rust/Python parallel workers: {parallel_workers}")
    print("Sample order: rotating byteseek and each baseline; output checked after every sample")
    print()


def main() -> None:
    args = parse_args()
    if args.warmups < 0 or args.iterations < 1 or args.parallel_workers < 1:
        raise ValueError("warmups must be non-negative; iterations and workers must be positive")

    os.environ["RAYON_NUM_THREADS"] = str(args.parallel_workers)
    config = BenchConfig(warmups=args.warmups, iterations=args.iterations)
    print_metadata(args.parallel_workers)
    with (
        concurrent.futures.ThreadPoolExecutor(max_workers=args.parallel_workers) as executor,
        tempfile.TemporaryDirectory(prefix="byteseek-bench-") as scratch,
    ):
        cases = [
            case
            for case in (
                *build_cases(args.parallel_workers, executor),
                # One worker reads files in a plain loop, as byteseek's one thread does.
                *build_structured_cases(
                    executor if args.parallel_workers > 1 else None, Path(scratch)
                ),
            )
            if args.filter in case.name
        ]
        validate_equivalence(cases)
        rows = run_benchmarks(cases, config)
    if args.csv is not None:
        write_csv(args.csv, rows, args.parallel_workers)


if __name__ == "__main__":
    main()
