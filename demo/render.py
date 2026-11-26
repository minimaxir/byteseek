"""Record VHS tapes and crop their screenshots to the text they draw.

Each tape is recorded taller than its output needs, then every screenshot is cropped
until the right and bottom margins mirror the left and top ones. The GIF that VHS
always records is deleted, as only the screenshots are kept.

Run from the repository root after `cargo build --release`:

    python demo/render.py demo/pretty.tape demo/context.tape demo/structured.tape \
        demo/fuzzy.tape
"""

import logging
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import NamedTuple

logger = logging.getLogger(__name__)

BACKGROUND_LIMIT = 40
_LIT = bytes(value > BACKGROUND_LIMIT for value in range(256))
_OUTPUT = re.compile(r"^Output\s+(\S+)", re.MULTILINE)
_SCREENSHOT = re.compile(r"^Screenshot\s+(\S+)", re.MULTILINE)
_PGM_HEADER = re.compile(rb"P5\s+(\d+)\s+(\d+)\s+255\s")


class Image(NamedTuple):
    """An 8-bit grayscale image, stored row by row."""

    width: int
    height: int
    pixels: bytes


def parse_pgm(pgm: bytes) -> Image:
    """Parse a binary 8-bit PGM image.

    Args:
        pgm: A ``P5`` image with a maximum value of 255.

    Returns:
        Its size and pixels.

    Raises:
        ValueError: If ``pgm`` is not an 8-bit ``P5`` image.
    """
    header = _PGM_HEADER.match(pgm)
    width, height = map(int, header.groups()) if header else (0, 0)
    if not header or len(pgm) - header.end() != width * height:
        raise ValueError("expected an 8-bit binary PGM image")
    return Image(width, height, pgm[header.end() :])


def tape_paths(pattern: re.Pattern[str], tape: str) -> list[Path]:
    r"""Return the paths a tape's ``Output`` or ``Screenshot`` commands write, in order.

    Example:
        >>> tape = 'Output demo/a.gif\nScreenshot "docs/a.png"'
        >>> [path.as_posix() for path in tape_paths(_SCREENSHOT, tape)]
        ['docs/a.png']
    """
    return [Path(path.strip("\"'")) for path in pattern.findall(tape)]


def content_box(image: Image) -> tuple[int, int, int, int]:
    """Bound the pixels brighter than the background.

    Args:
        image: The image to search.

    Returns:
        ``(left, top, right, bottom)``, exclusive on the right and bottom.

    Raises:
        ValueError: If every pixel is background.
    """
    lit = image.pixels.translate(_LIT)
    width = image.width
    spans = [
        (row, line.find(1), line.rfind(1) + 1)
        for row in range(image.height)
        if 1 in (line := lit[row * width : (row + 1) * width])
    ]
    if not spans:
        raise ValueError("the screenshot shows nothing")
    return (
        min(left for _, left, _ in spans),
        spans[0][0],
        max(right for _, _, right in spans),
        spans[-1][0] + 1,
    )


def crop_size(
    box: tuple[int, int, int, int], width: int, height: int
) -> tuple[int, int]:
    """Size a top-left crop whose right and bottom margins mirror the left and top.

    Args:
        box: ``(left, top, right, bottom)`` of the content.
        width: Image width.
        height: Image height.

    Returns:
        ``(width, height)``, within the image.

    Example:
        >>> crop_size((32, 40, 1045, 300), 1120, 364)
        (1077, 340)
    """
    left, top, right, bottom = box
    return min(width, right + left), min(height, bottom + top)


def _run(*command: str) -> bytes:
    return subprocess.run(command, check=True, capture_output=True).stdout  # noqa: S603


def grayscale(png: Path) -> Image:
    """Return ``png`` as a grayscale image."""
    with tempfile.TemporaryDirectory() as folder:
        path = Path(folder, "gray.pgm")
        _run("ffmpeg", "-v", "error", "-y", "-i", str(png), "-pix_fmt", "gray", str(path))
        return parse_pgm(path.read_bytes())


def crop_png(png: Path, width: int, height: int) -> None:
    """Crop a PNG in place to its top-left ``width`` by ``height``."""
    cropped = png.with_suffix(".cropped.png")
    _run(
        "ffmpeg", "-v", "error", "-y", "-i", str(png),
        "-vf", f"crop={width}:{height}:0:0", str(cropped),
    )  # fmt: skip
    cropped.replace(png)


def render(tape: Path) -> list[tuple[Path, int, int]]:
    """Record ``tape`` with VHS, crop its screenshots, and delete its recordings.

    Args:
        tape: Path of the VHS tape, run from the current directory.

    Returns:
        Each screenshot with its cropped width and height.

    Raises:
        ValueError: If the tape takes no screenshot, or one shows nothing.
        subprocess.CalledProcessError: If VHS or FFmpeg fails.
    """
    text = tape.read_text()
    screenshots = tape_paths(_SCREENSHOT, text)
    if not screenshots:
        raise ValueError(f"{tape} takes no screenshot")
    _run("vhs", str(tape))
    for recording in tape_paths(_OUTPUT, text):
        recording.unlink(missing_ok=True)
    cropped = []
    for screenshot in screenshots:
        image = grayscale(screenshot)
        width, height = crop_size(content_box(image), image.width, image.height)
        crop_png(screenshot, width, height)
        cropped.append((screenshot, width, height))
    return cropped


def main(tapes: list[str]) -> int:
    """Render each tape, logging any failure.

    Args:
        tapes: Paths of the VHS tapes.

    Returns:
        The process exit code: 1 if any tape failed, else 0.
    """
    logging.basicConfig(format="%(message)s", level=logging.INFO)
    failed = False
    for tape in map(Path, tapes):
        try:
            screenshots = render(tape)
        except subprocess.CalledProcessError as error:
            stderr = error.stderr.decode(errors="replace").strip()
            logger.error(f"{tape}: {' '.join(error.cmd)} failed: {stderr}")
            failed = True
        except (OSError, ValueError) as error:
            logger.error(f"{tape}: {error}")
            failed = True
        else:
            for screenshot, width, height in screenshots:
                logger.info(f"{screenshot}: cropped to {width}x{height}")
    return int(failed)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
