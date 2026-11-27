from __future__ import annotations

import sys
import threading
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benchmarks"))
# The workloads need the `bench` dependency group.
workloads = pytest.importorskip("structured_workloads")


def test_map_files_runs_on_the_calling_thread_without_an_executor() -> None:
    files = ["a.log", "b.log"]

    threads = list(workloads.map_files(lambda _: threading.get_ident(), files, None))

    assert threads == [threading.get_ident()] * len(files)


def test_map_files_keeps_file_order_with_and_without_an_executor() -> None:
    files = ["a.log", "b.log", "c.log"]

    with ThreadPoolExecutor(max_workers=2) as executor:
        pooled = list(workloads.map_files(str.upper, files, executor))
    inline = list(workloads.map_files(str.upper, files, None))

    assert pooled == inline == ["A.LOG", "B.LOG", "C.LOG"]


# The comparison needs the `bench` dependency group too.
comparison = pytest.importorskip("compare_benchmarks")


def test_median_interval_narrows_as_samples_are_added() -> None:
    few = list(range(1, 6))
    many = list(range(1, 32))

    few_bounds = comparison.median_interval(few)
    many_bounds = comparison.median_interval(many)

    # Five samples are too few for the level, so the bounds are the extremes.
    assert few_bounds == (1, 5)
    # Of 31 samples, the median lies between the 10th and the 22nd 97% of the time.
    assert many_bounds == (10, 22)
    assert comparison.median_interval([7]) == (7, 7)


def test_benchmark_case_times_every_runner_with_bounds_around_its_median() -> None:
    baselines = (("first", lambda: 1), ("second", lambda: 1))
    case = comparison.BenchmarkCase("same", lambda: 1, baselines)

    timings = comparison.benchmark_case(case, comparison.BenchConfig(warmups=1, iterations=9))

    assert len(timings) == 3
    assert all(timing.lower_ns <= timing.median_ns <= timing.upper_ns for timing in timings)


def test_csv_rows_end_with_the_bounds_of_both_timings(tmp_path: Path) -> None:
    row = comparison.ResultRow(
        "case",
        "re",
        comparison.Timing(2_000_000, 1_500_000, 2_500_000),
        comparison.Timing(8_000_000, 7_000_000, 9_000_000),
    )
    path = tmp_path / "results.csv"

    comparison.write_csv(path, [row], workers=1)

    header, line = path.read_text(encoding="utf-8").splitlines()
    assert header.split(",")[4:] == [
        "byteseek_ms",
        "python_ms",
        "speedup_byteseek_over_python",
        "byteseek_ms_ci_lower",
        "byteseek_ms_ci_upper",
        "python_ms_ci_lower",
        "python_ms_ci_upper",
        "confidence_level",
    ]
    assert line.split(",")[4:] == [
        "2.00000",
        "8.00000",
        "4.00",
        "1.50000",
        "2.50000",
        "7.00000",
        "9.00000",
        "0.95",
    ]
