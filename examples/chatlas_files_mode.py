"""Use chatlas with mcp-repl overflow files mode."""

import asyncio
from pathlib import Path
from typing import Optional

from chatlas import ChatOpenAI


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


async def main() -> None:
    chat = ChatOpenAI(
        system_prompt=(
            "Use the `repl` MCP tool for Python execution. This mcp-repl "
            "server runs in overflow files mode. When a reply discloses an "
            "output bundle path, inspect it with `list_directory` and "
            "`read_text_file`. Prefer explicit line ranges when reading large "
            "files; `read_text_file` does not impose a fixed size limit."
        ),
    )
    chat.register_tool(list_directory)
    chat.register_tool(read_text_file)

    try:
        await chat.register_mcp_tools_stdio_async(
            name="mcp_repl_files",
            command="mcp-repl",
            args=[
                "--sandbox",
                "workspace-write",
                "--oversized-output",
                "files",
                "--interpreter",
                "python",
            ],
            include_tools=["repl"],
        )

        response = await chat.chat_async(
            "Use the repl tool to print 2500 numbered lines in the format "
            "bundle-record-0001 through bundle-record-2500 so the output "
            "overflows to files mode. Then inspect the disclosed bundle: list "
            "the bundle directory, read transcript.txt with line ranges until "
            "you find bundle-record-1750, and report the matching line.",
            echo="none",
        )
        print(await response.get_content())
    finally:
        await chat.cleanup_mcp_tools()


if __name__ == "__main__":
    asyncio.run(main())
