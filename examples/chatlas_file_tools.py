"""Ordinary chatlas tools used by the files-mode example."""

from pathlib import Path
from typing import Optional


def list_directory(path: str) -> str:
    """
    List the direct children of a directory.

    Parameters
    ----------
    path:
        Absolute or relative directory path to list.
    """
    directory = Path(path).expanduser()
    assert directory.is_dir()

    rows = []
    for entry in sorted(directory.iterdir(), key=lambda value: value.name):
        kind = "dir" if entry.is_dir() else "file"
        rows.append(f"{kind}\t{entry.stat().st_size}\t{entry.name}")
    return "\n".join(rows)


def read_text_file(
    path: str,
    start_line: int = 1,
    end_line: Optional[int] = None,
) -> str:
    """
    Read a UTF-8 text file, optionally by inclusive line range.

    Parameters
    ----------
    path:
        Absolute or relative text file path to read.
    start_line:
        First 1-based line number to read.
    end_line:
        Last 1-based line number to read. Leave unset to read through EOF.
        This function has no artificial size limit.
    """
    assert start_line >= 1
    assert end_line is None or end_line >= start_line

    target = Path(path).expanduser()
    assert target.is_file()

    output = []
    with target.open("r", encoding="utf-8", errors="replace") as handle:
        for line_number, line in enumerate(handle, start=1):
            if line_number < start_line:
                continue
            if end_line is not None and line_number > end_line:
                break
            output.append(f"{line_number}: {line}")

    return "".join(output)
