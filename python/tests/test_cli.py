from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Sequence
from importlib.metadata import version
from pathlib import Path

import pytest

from byteseek.__main__ import main
from byteseek._byteseek import run_cli

MODULE = (sys.executable, "-m", "byteseek")
SCRIPT = shutil.which("byteseek", path=str(Path(sys.executable).parent))
LAUNCHERS = pytest.mark.parametrize(
    "launcher",
    [MODULE, (SCRIPT,)],
    ids=["python -m byteseek", "console script"],
)
POSIX_ONLY = pytest.mark.skipif(sys.platform == "win32", reason="POSIX signals and FIFOs")


@pytest.fixture
def logs(tmp_path: Path) -> Path:
    path = tmp_path / "app.log"
    path.write_text("alpha\nbeta error\ngamma\n")
    return path


def run_main(argv: Sequence[str], capfd: pytest.CaptureFixture[str]) -> tuple[int, str, str]:
    """Runs `main` in this process and returns its status, stdout, and stderr."""
    with pytest.raises(SystemExit) as exited:
        main(argv)
    captured = capfd.readouterr()
    assert isinstance(exited.value.code, int)
    return exited.value.code, captured.out, captured.err


def run_process(
    launcher: Sequence[str | None], *args: str, stdin: bytes = b""
) -> subprocess.CompletedProcess[bytes]:
    command = [part for part in launcher if part is not None]
    return subprocess.run(
        [*command, *args], input=stdin, capture_output=True, timeout=60, check=False
    )


def test_console_script_is_installed() -> None:
    assert SCRIPT is not None


def test_main_prints_selected_lines_and_exits_zero(
    logs: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    assert run_main(["-n", "error", str(logs)], capfd) == (0, "2:beta error\n", "")


def test_main_exits_one_when_nothing_is_selected(
    logs: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    assert run_main(["missing", str(logs)], capfd) == (1, "", "")


def test_main_reads_sys_argv_by_default(
    logs: Path, capfd: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(sys, "argv", ["byteseek", "--count", "a", str(logs)])

    with pytest.raises(SystemExit) as exited:
        main()

    assert exited.value.code == 0
    assert capfd.readouterr().out == "3\n"


def test_main_accepts_any_sequence_of_arguments(
    logs: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    assert run_main(("--count", "gamma", str(logs)), capfd) == (0, "1\n", "")


def test_usage_error_exits_two_and_leaves_the_interpreter_running(
    logs: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    status, out, err = run_main(["--no-such-flag"], capfd)

    assert (status, out) == (2, "")
    assert "unexpected argument '--no-such-flag'" in err
    assert run_main(["-c", "beta", str(logs)], capfd) == (0, "1\n", "")


def test_missing_file_reports_an_error_and_exits_two(
    tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    status, out, err = run_main(["alpha", str(tmp_path / "absent.log")], capfd)

    assert (status, out) == (2, "")
    assert "absent.log" in err


def test_help_prints_usage_and_exits_zero(capfd: pytest.CaptureFixture[str]) -> None:
    status, out, err = run_main(["--help"], capfd)

    assert (status, err) == (0, "")
    assert "Usage: byteseek" in out


def test_version_matches_the_installed_package(capfd: pytest.CaptureFixture[str]) -> None:
    assert run_main(["--version"], capfd) == (0, f"byteseek {version('byteseek')}\n", "")


def test_output_without_a_final_newline_is_flushed(
    logs: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    assert run_main(["--files-with-matches", "--null", "beta", str(logs)], capfd) == (
        0,
        f"{logs}\0",
        "",
    )


def test_non_ascii_patterns_and_paths_pass_through(
    tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    path = tmp_path / "naïve café.txt"
    path.write_text("tea\ncafé au lait\n")

    assert run_main(["café", str(path)], capfd) == (0, "café au lait\n", "")


def test_arguments_that_are_not_utf8_reach_the_command_line_as_bytes(
    logs: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    status, _, err = run_main([os.fsdecode(b"caf\xe9"), str(logs)], capfd)

    assert status == 2
    assert "pattern is not valid UTF-8" in err


def test_main_restores_the_interrupt_handler(
    logs: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    before = signal.getsignal(signal.SIGINT)

    run_main(["alpha", str(logs)], capfd)

    assert signal.getsignal(signal.SIGINT) is before


@LAUNCHERS
def test_launchers_search_files(launcher: Sequence[str | None], logs: Path) -> None:
    completed = run_process(launcher, "-n", "error", str(logs))

    assert (completed.returncode, completed.stdout, completed.stderr) == (0, b"2:beta error\n", b"")


@LAUNCHERS
def test_launchers_search_standard_input(launcher: Sequence[str | None]) -> None:
    completed = run_process(launcher, "-n", "beta", stdin=b"alpha\nbeta\n")

    assert (completed.returncode, completed.stdout) == (0, b"2:beta\n")


@LAUNCHERS
def test_launchers_exit_with_ripgrep_statuses(
    launcher: Sequence[str | None], logs: Path
) -> None:
    assert run_process(launcher, "missing", str(logs)).returncode == 1
    assert run_process(launcher, "--no-such-flag").returncode == 2


def test_structured_paths_are_searched(tmp_path: Path) -> None:
    document = tmp_path / "config.json"
    document.write_text('{"server": {"port": 8080}}\n')

    completed = run_process(MODULE, "$.server.port", str(document))

    assert (completed.returncode, completed.stdout) == (0, b"$.server.port:8080\n")


@pytest.mark.parametrize("threads", ["1", "4"])
def test_thread_counts_search_directories(tmp_path: Path, threads: str) -> None:
    for name in ("a.log", "b.log", "c.log"):
        (tmp_path / name).write_text("ok\nerror\nerror again\n")

    completed = run_process(
        MODULE, "--threads", threads, "--sort", "path", "--count", "error", str(tmp_path)
    )

    expected = "".join(f"{tmp_path / name}:2\n" for name in ("a.log", "b.log", "c.log"))
    assert (completed.returncode, completed.stdout.decode()) == (0, expected)


def test_closed_output_pipe_ends_the_search_quietly(tmp_path: Path) -> None:
    path = tmp_path / "big.log"
    path.write_text("".join(f"error {index}\n" for index in range(200_000)))

    with subprocess.Popen(
        [*MODULE, "error", str(path)], stdout=subprocess.PIPE, stderr=subprocess.PIPE
    ) as process:
        assert process.stdout is not None and process.stderr is not None
        assert process.stdout.readline() == b"error 0\n"
        process.stdout.close()
        stderr = process.stderr.read()
        status = process.wait(timeout=60)

    assert (status, stderr) == (0, b"")


@POSIX_ONLY
def test_interrupt_ends_a_running_search() -> None:
    # Standard input stays open, so the search blocks in native code until interrupted.
    with subprocess.Popen(
        [*MODULE, "--count", "error"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ) as process:
        time.sleep(1)
        process.send_signal(signal.SIGINT)
        try:
            status = process.wait(timeout=10)
        finally:
            process.kill()

    assert status == -signal.SIGINT


@POSIX_ONLY
def test_run_cli_releases_the_gil_while_searching(
    tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    fifo = tmp_path / "input.fifo"
    os.mkfifo(fifo)
    statuses: list[int] = []
    searching = threading.Thread(
        target=lambda: statuses.append(run_cli(["--count", "error", str(fifo)]))
    )

    searching.start()
    # The search blocks opening the FIFO; writing to it needs this thread to run Python.
    with fifo.open("w") as writer:
        writer.write("error\nok\nerror\n")
    searching.join(timeout=10)

    assert statuses == [0]
    assert capfd.readouterr().out == "2\n"
