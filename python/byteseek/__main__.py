"""The ``byteseek`` command line, run by the ``byteseek`` console script and ``python -m byteseek``."""

from __future__ import annotations

import signal
import sys

from byteseek._byteseek import run_cli

# Annotations only, so starting the command line doesn't import `typing`.
TYPE_CHECKING = False
if TYPE_CHECKING:
    from collections.abc import Sequence
    from typing import NoReturn


def main(argv: Sequence[str] | None = None) -> NoReturn:
    """Run the ``byteseek`` command line and exit with its status.

    Results go to the process's standard output and diagnostics to its standard error, as
    they would from the native binary. Call it from the main thread.

    Args:
        argv: Arguments after the program name; ``sys.argv[1:]`` when omitted.

    Raises:
        SystemExit: Always, with ripgrep's status: 0 if a line was selected, 1 if none was,
            and 2 after an error.
    """
    args = sys.argv[1:] if argv is None else list(argv)
    # Python acts on Ctrl-C only between bytecodes, so a long search would ignore it; the
    # default action ends the search at once, as it ends the native binary.
    previous = signal.signal(signal.SIGINT, signal.SIG_DFL)
    try:
        status = run_cli(args)
    finally:
        signal.signal(signal.SIGINT, previous)
    sys.exit(status)


if __name__ == "__main__":
    main()
