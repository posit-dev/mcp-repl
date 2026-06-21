"""Synchronous mcp-repl tool for chatlas examples."""

from __future__ import annotations

import json
import queue
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Optional, Sequence


PROTOCOL_VERSION = "2025-06-18"


class McpReplTool:
    """
    Run code in a persistent mcp-repl session.

    Parameters
    ----------
    input:
        Code to execute. In pager mode, send empty input to advance one page,
        `:/pattern` to search, or `:q` to exit the pager.
    timeout_ms:
        Maximum milliseconds to wait for this tool call. A timeout does not
        cancel backend work; call again with empty input to poll.
    """

    def __init__(
        self,
        args: Sequence[str],
        command: str = "mcp-repl",
        timeout_seconds: float = 60.0,
    ) -> None:
        assert timeout_seconds > 0
        self.__name__ = "repl"
        self.command = command
        self.args = list(args)
        self.timeout_seconds = timeout_seconds
        self.next_request_id = 1
        self.stderr_lines: list[str] = []
        self.stdout_lines: queue.Queue[Optional[bytes]] = queue.Queue()
        self.process: Optional[subprocess.Popen[bytes]] = None
        self.stdout_thread: Optional[threading.Thread] = None
        self.stderr_thread: Optional[threading.Thread] = None

    def start(self) -> "McpReplTool":
        """Start mcp-repl and initialize the MCP session."""
        assert self.process is None
        self.process = subprocess.Popen(
            [self.command, *self.args],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        assert self.process.stdout is not None
        assert self.process.stderr is not None
        self.stdout_thread = threading.Thread(
            target=self._collect_stdout,
            args=(self.process.stdout,),
            daemon=True,
        )
        self.stderr_thread = threading.Thread(
            target=self._collect_stderr,
            args=(self.process.stderr,),
            daemon=True,
        )
        self.stdout_thread.start()
        self.stderr_thread.start()
        self._initialize()
        return self

    def __call__(self, input: str, timeout_ms: int = 10000) -> str:
        assert timeout_ms > 0
        result = self._request(
            "tools/call",
            {
                "name": "repl",
                "arguments": {
                    "input": input,
                    "timeout_ms": timeout_ms,
                },
            },
        )
        content = result.get("result", {}).get("content")
        assert isinstance(content, list)
        chunks = []
        for item in content:
            assert isinstance(item, dict)
            if item.get("type") == "text":
                chunks.append(str(item.get("text", "")))
            else:
                chunks.append(f"[non-text MCP content: {item.get('type')}]")
        return "".join(chunks)

    def close(self) -> None:
        process = self.process
        if process is None:
            return
        if process.poll() is None:
            if process.stdin is not None:
                process.stdin.close()
            process.terminate()
            try:
                process.wait(timeout=3.0)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3.0)
        if self.stdout_thread is not None:
            self.stdout_thread.join(timeout=1.0)
        if self.stderr_thread is not None:
            self.stderr_thread.join(timeout=1.0)
        self.process = None

    def _initialize(self) -> None:
        self._request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "chatlas-mcp-repl-tool-example",
                    "version": "0.1.0",
                },
            },
        )
        self._write_message(
            {
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {},
            }
        )

    def _request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        request_id = self.next_request_id
        self.next_request_id += 1
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
        process = self.process
        assert process is not None
        assert process.stdin is not None
        if process.poll() is not None:
            raise RuntimeError(
                f"mcp-repl exited before write: code={process.returncode}\n"
                f"{self._stderr_tail()}"
            )
        payload = json.dumps(message, separators=(",", ":")).encode("utf-8") + b"\n"
        process.stdin.write(payload)
        process.stdin.flush()

    def _read_response(self, request_id: int) -> dict[str, Any]:
        deadline = time.monotonic() + self.timeout_seconds
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError(
                    f"timed out waiting for mcp-repl response id {request_id}\n"
                    f"{self._stderr_tail()}"
                )
            try:
                raw = self.stdout_lines.get(timeout=remaining)
            except queue.Empty as exc:
                raise RuntimeError(
                    f"timed out waiting for mcp-repl response id {request_id}\n"
                    f"{self._stderr_tail()}"
                ) from exc
            if raw is None:
                raise RuntimeError(
                    f"mcp-repl stdout closed before response id {request_id}\n"
                    f"{self._stderr_tail()}"
                )
            if raw.strip() == b"":
                continue
            message = json.loads(raw)
            if message.get("id") != request_id:
                raise RuntimeError(
                    "unexpected mcp-repl message while waiting for "
                    f"id {request_id}: {message!r}"
                )
            if "error" in message:
                raise RuntimeError(f"mcp-repl error: {message['error']!r}")
            return message

    def _stderr_tail(self) -> str:
        tail = [line for line in self.stderr_lines[-20:] if line]
        if not tail:
            return "mcp-repl stderr: <empty>"
        return "mcp-repl stderr:\n" + "\n".join(tail)

    def _collect_stdout(self, stream: Any) -> None:
        for raw in iter(stream.readline, b""):
            self.stdout_lines.put(raw)
        self.stdout_lines.put(None)

    def _collect_stderr(self, stream: Any) -> None:
        for raw in iter(stream.readline, b""):
            self.stderr_lines.append(raw.decode("utf-8", errors="replace").rstrip())


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
