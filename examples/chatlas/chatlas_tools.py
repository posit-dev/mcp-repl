"""Common helpers used by the chatlas examples."""

from __future__ import annotations

import atexit
import json
import os
import queue
import subprocess
import sys
import threading
import time
from enum import Enum
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Literal, Optional

if TYPE_CHECKING:
    from chatlas import ChatOpenAI


class OverflowMode(str, Enum):
    PAGER = "pager"
    FILES = "files"


def _mcp_repl_env() -> dict[str, str]:
    assert sys.executable
    return {
        **os.environ,
        "MCP_REPL_PYTHON_EXECUTABLE": sys.executable,
    }


async def register_tool_repl(chat: ChatOpenAI, overflow: OverflowMode) -> None:
    assert isinstance(overflow, OverflowMode)

    await chat.register_mcp_tools_stdio_async(
        name=f"mcp_repl_{overflow.value}",
        command="mcp-repl",
        args=[
            "--sandbox",
            "workspace-write",
            "--oversized-output",
            overflow.value,
            "--interpreter",
            "python",
        ],
        include_tools=["repl"],
        transport_kwargs={"env": _mcp_repl_env()},
    )


class McpReplError(RuntimeError):
    pass


class _McpReplClient:
    def __init__(self, overflow: Literal["pager", "files"]) -> None:
        assert overflow in ("pager", "files")

        self._next_request_id = 1
        self._lock = threading.Lock()
        self._stdout_lines: queue.Queue[str | None] = queue.Queue()
        self._stderr_lines: list[str] = []
        self._process = subprocess.Popen(
            [
                "mcp-repl",
                "--sandbox",
                "workspace-write",
                "--oversized-output",
                overflow,
                "--interpreter",
                "python",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
            env=_mcp_repl_env(),
        )
        assert self._process.stdin is not None
        assert self._process.stdout is not None
        assert self._process.stderr is not None

        self._stdout_thread = threading.Thread(
            target=self._collect_stdout,
            args=(self._process.stdout,),
            daemon=True,
        )
        self._stderr_thread = threading.Thread(
            target=self._collect_stderr,
            args=(self._process.stderr,),
            daemon=True,
        )
        self._stdout_thread.start()
        self._stderr_thread.start()
        self._initialize()
        atexit.register(self.close)

    def close(self) -> None:
        if self._process.poll() is not None:
            return
        assert self._process.stdin is not None
        self._process.stdin.close()
        self._process.terminate()
        try:
            self._process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self._process.kill()
            self._process.wait(timeout=3)

    def call_repl(self, input: str, timeout_ms: Optional[int] = None) -> str:
        assert isinstance(input, str)
        assert timeout_ms is None or timeout_ms > 0

        arguments: dict[str, Any] = {"input": input}
        if timeout_ms is not None:
            arguments["timeout_ms"] = timeout_ms

        result = self._request(
            "tools/call",
            {
                "name": "repl",
                "arguments": arguments,
            },
        )
        return _tool_result_text(result)

    def _initialize(self) -> None:
        result = self._request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "chatlas-mcp-repl-sync-example",
                    "version": "0.1.0",
                },
            },
        )
        assert isinstance(result, dict)
        self._notify("notifications/initialized", {})

    def _notify(self, method: str, params: dict[str, Any]) -> None:
        self._write_message(
            {
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }
        )

    def _request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        with self._lock:
            request_id = self._next_request_id
            self._next_request_id += 1
            self._write_message(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": params,
                }
            )
            return self._read_response(request_id)

    def _write_message(self, message: dict[str, Any]) -> None:
        if self._process.poll() is not None:
            raise McpReplError(
                f"mcp-repl exited before write: code={self._process.returncode}\n"
                f"{self._stderr_tail()}"
            )
        assert self._process.stdin is not None
        payload = json.dumps(message, separators=(",", ":"))
        self._process.stdin.write(payload + "\n")
        self._process.stdin.flush()

    def _read_response(self, request_id: int) -> dict[str, Any]:
        deadline = time.monotonic() + 30
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise McpReplError(
                    f"timed out waiting for response id {request_id}\n"
                    f"{self._stderr_tail()}"
                )
            try:
                raw = self._stdout_lines.get(timeout=remaining)
            except queue.Empty as exc:
                raise McpReplError(
                    f"timed out waiting for response id {request_id}\n"
                    f"{self._stderr_tail()}"
                ) from exc

            if raw is None:
                raise McpReplError(
                    f"mcp-repl stdout closed before response id {request_id}; "
                    f"code={self._process.poll()}\n{self._stderr_tail()}"
                )
            if raw.strip() == "":
                continue
            message = json.loads(raw)
            if message.get("id") != request_id:
                raise McpReplError(
                    f"unexpected mcp-repl message while waiting for id "
                    f"{request_id}: {message!r}"
                )
            if "error" in message:
                raise McpReplError(
                    f"mcp-repl returned error for {request_id}: "
                    f"{message['error']!r}\n{self._stderr_tail()}"
                )
            result = message.get("result")
            assert isinstance(result, dict)
            return result

    def _collect_stdout(self, stream: Any) -> None:
        for line in stream:
            self._stdout_lines.put(line)
        self._stdout_lines.put(None)

    def _collect_stderr(self, stream: Any) -> None:
        for line in stream:
            self._stderr_lines.append(line.rstrip("\n"))

    def _stderr_tail(self) -> str:
        tail = [line for line in self._stderr_lines[-20:] if line]
        if not tail:
            return "mcp-repl stderr: <empty>"
        return "mcp-repl stderr:\n" + "\n".join(tail)


def _tool_result_text(result: dict[str, Any]) -> str:
    content = result.get("content")
    assert isinstance(content, list)

    parts = []
    for item in content:
        assert isinstance(item, dict)
        if item.get("type") == "text":
            text = item.get("text")
            assert isinstance(text, str)
            parts.append(text)
        else:
            parts.append(json.dumps(item, ensure_ascii=False))
    return "\n".join(parts)


def repl_tools(overflow: Literal["pager", "files"]) -> list[Callable[..., str]]:
    client = _McpReplClient(overflow)

    def repl(input: str, timeout_ms: Optional[int] = None) -> str:
        """
        Run input in the live mcp-repl Python REPL.

        Parameters
        ----------
        input:
            Python code, stdin text, or pager command to send to mcp-repl.
        timeout_ms:
            Optional timeout for this mcp-repl request.
        """
        return client.call_repl(input=input, timeout_ms=timeout_ms)

    return [repl]


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
