#!/usr/bin/env python3
from __future__ import annotations

import argparse
import difflib
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any


PROTOCOL_VERSION = "2025-06-18"
DEFAULT_TIMEOUT_SECONDS = 35.0


class SuiteFailure(Exception):
    pass


class McpProtocolError(SuiteFailure):
    pass


def collect_stderr(stream: Any, sink: list[str]) -> None:
    for raw in iter(stream.readline, b""):
        sink.append(raw.decode("utf-8", errors="replace").rstrip())


def collect_stdout(stream: Any, sink: queue.Queue[bytes | None]) -> None:
    for raw in iter(stream.readline, b""):
        sink.put(raw)
    sink.put(None)


class McpStdioClient:
    def __init__(
        self,
        binary: Path,
        server_args: Sequence[str],
        server_env: Sequence[tuple[str, str]],
        timeout_seconds: float,
    ) -> None:
        assert timeout_seconds > 0
        self.binary = binary
        self.server_args = list(server_args)
        self.server_env = dict(server_env)
        self.timeout_seconds = timeout_seconds
        self.next_request_id = 1
        self.stderr_lines: list[str] = []
        self.stdout_lines: queue.Queue[bytes | None] = queue.Queue()
        self.process: subprocess.Popen[bytes] | None = None
        self.stdout_thread: threading.Thread | None = None
        self.stderr_thread: threading.Thread | None = None

    def __enter__(self) -> McpStdioClient:
        if not self.binary.is_file():
            raise SuiteFailure(f"binary does not exist: {self.binary}")

        env = os.environ.copy()
        for key in [
            "R_PROFILE_USER",
            "R_PROFILE_SITE",
            "R_ENVIRON",
            "R_ENVIRON_USER",
            "MCP_REPL_UPDATE_PLOT_IMAGES",
        ]:
            env.pop(key, None)
        env.update(self.server_env)

        command = [str(self.binary), *self.server_args]
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        assert self.process.stdout is not None
        assert self.process.stderr is not None
        self.stdout_thread = threading.Thread(
            target=collect_stdout,
            args=(self.process.stdout, self.stdout_lines),
            daemon=True,
        )
        self.stderr_thread = threading.Thread(
            target=collect_stderr,
            args=(self.process.stderr, self.stderr_lines),
            daemon=True,
        )
        self.stdout_thread.start()
        self.stderr_thread.start()
        self.initialize()
        return self

    def __exit__(self, *_exc_info: object) -> None:
        self.close()

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

    def initialize(self) -> None:
        response = self.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "mcp-repl-public-api-suite",
                    "version": "0.1.0",
                },
            },
        )
        result = response.get("result")
        if not isinstance(result, dict):
            raise McpProtocolError(f"initialize returned non-object result: {response!r}")
        self.notify("notifications/initialized", {})

    def notify(self, method: str, params: dict[str, Any]) -> None:
        self.write_message(
            {
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }
        )

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        response = self.request(
            "tools/call",
            {
                "name": name,
                "arguments": arguments,
            },
        )
        result = response.get("result")
        if not isinstance(result, dict):
            raise McpProtocolError(f"tools/call returned non-object result: {response!r}")
        return result

    def repl(self, input: str, *, timeout_ms: int | None = None) -> dict[str, Any]:
        arguments: dict[str, Any] = {"input": input}
        if timeout_ms is not None:
            arguments["timeout_ms"] = timeout_ms
        return self.call_tool("repl", arguments)

    def repl_reset(self) -> dict[str, Any]:
        return self.call_tool("repl_reset", {})

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        request_id = self.next_request_id
        self.next_request_id += 1
        self.write_message(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }
        )
        return self.read_response(request_id)

    def write_message(self, message: dict[str, Any]) -> None:
        process = self.process
        assert process is not None
        assert process.stdin is not None
        if process.poll() is not None:
            raise McpProtocolError(
                f"server exited before write: code={process.returncode}\n{self.stderr_tail()}"
            )
        payload = json.dumps(message, separators=(",", ":")).encode("utf-8") + b"\n"
        process.stdin.write(payload)
        process.stdin.flush()

    def read_response(self, request_id: int) -> dict[str, Any]:
        deadline = time.monotonic() + self.timeout_seconds
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise McpProtocolError(
                    f"timed out waiting for response id {request_id}\n{self.stderr_tail()}"
                )
            try:
                raw = self.stdout_lines.get(timeout=remaining)
            except queue.Empty as exc:
                raise McpProtocolError(
                    f"timed out waiting for response id {request_id}\n{self.stderr_tail()}"
                ) from exc
            if raw is None:
                process = self.process
                code = None if process is None else process.poll()
                raise McpProtocolError(
                    f"server stdout closed before response id {request_id}; code={code}\n"
                    f"{self.stderr_tail()}"
                )
            if raw.strip() == b"":
                continue
            try:
                message = json.loads(raw)
            except json.JSONDecodeError as exc:
                raise McpProtocolError(
                    f"server wrote non-JSON stdout line: {raw!r}\n{self.stderr_tail()}"
                ) from exc
            if message.get("id") != request_id:
                raise McpProtocolError(
                    f"unexpected server message while waiting for id {request_id}: {message!r}"
                )
            if "error" in message:
                raise McpProtocolError(
                    f"server returned error for {request_id}: {message['error']!r}"
                )
            return message

    def stderr_tail(self) -> str:
        tail = [line for line in self.stderr_lines[-20:] if line]
        if not tail:
            return "server stderr: <empty>"
        return "server stderr:\n" + "\n".join(tail)


def text(value: str) -> dict[str, Any]:
    return {"type": "text", "text": value}


def tool_result(*content: dict[str, Any], is_error: bool = False) -> dict[str, Any]:
    return {"content": list(content), "isError": is_error}


def pretty_json(value: dict[str, Any]) -> str:
    return json.dumps(value, indent=2, ensure_ascii=False, sort_keys=True)


def assert_identical(
    expected: dict[str, Any],
    received: dict[str, Any],
    context: str,
) -> None:
    if expected == received:
        return
    diff = "\n".join(
        difflib.unified_diff(
            pretty_json(expected).splitlines(),
            pretty_json(received).splitlines(),
            fromfile="expected",
            tofile="received",
            lineterm="",
        )
    )
    raise SuiteFailure(f"{context} response mismatch:\n{diff}")


def result_text(result: dict[str, Any]) -> str:
    content = result.get("content")
    if not isinstance(content, list):
        raise SuiteFailure(f"tool result content is not a list: {result!r}")
    chunks: list[str] = []
    for item in content:
        if isinstance(item, dict) and isinstance(item.get("text"), str):
            chunks.append(item["text"])
    return "".join(chunks)


def is_busy_response(result: dict[str, Any]) -> bool:
    response_text = result_text(result)
    return (
        "<<repl status: busy" in response_text
        or "worker is busy" in response_text
        or "request already running" in response_text
        or "input discarded while worker busy" in response_text
    )


def wait_for_response(
    client: McpStdioClient,
    expected: dict[str, Any],
    context: str,
    deadline_seconds: float = 5.0,
) -> dict[str, Any]:
    assert deadline_seconds > 0
    deadline = time.monotonic() + deadline_seconds
    last_received: dict[str, Any] | None = None
    while time.monotonic() < deadline:
        received = client.repl("", timeout_ms=500)
        last_received = received
        if is_busy_response(received):
            continue
        assert_identical(expected, normalize_response(received), context)
        return received
    raise SuiteFailure(f"{context} remained busy after polling: {last_received!r}")


def disclosed_path(text: str, suffix: str) -> Path | None:
    end = text.find(suffix)
    if end == -1:
        return None
    end += len(suffix)
    start = 0
    for index in range(end - 1, -1, -1):
        ch = text[index]
        if ch.isspace() or ch in "\"'[(":
            start = index + 1
            break
    return Path(text[start:end])


def bundle_transcript_path(text: str) -> Path | None:
    return disclosed_path(text, "transcript.txt")


def normalize_busy_timeout_elapsed_ms(value: str) -> str:
    marker = "elapsed_ms="
    out: list[str] = []
    index = 0
    while True:
        marker_index = value.find(marker, index)
        if marker_index == -1:
            out.append(value[index:])
            return "".join(out)
        out.append(value[index:marker_index])
        out.append(marker)
        digit_index = marker_index + len(marker)
        while digit_index < len(value) and value[digit_index].isdigit():
            digit_index += 1
        if digit_index > marker_index + len(marker):
            out.append("N")
        index = digit_index


def disclosed_path_strings(value: str, suffix: str) -> list[str]:
    paths: list[str] = []
    search_from = 0
    while True:
        end = value.find(suffix, search_from)
        if end == -1:
            return paths
        end += len(suffix)
        start = 0
        for index in range(end - 1, -1, -1):
            ch = value[index]
            if ch.isspace() or ch in "\"'[(":
                start = index + 1
                break
        paths.append(value[start:end])
        search_from = end


def normalize_output_bundle_path(path: str) -> str:
    parts = path.replace("\\", "/").split("/")
    output_dir = next(
        (part for part in reversed(parts) if part.startswith("output-")),
        "output-0001",
    )
    leaf = "events.log" if path.endswith("events.log") else "transcript.txt"
    return f"<mcp-repl-output>/{output_dir}/{leaf}"


def normalize_output_bundle_paths(value: str) -> str:
    normalized = value
    for suffix in ["transcript.txt", "events.log"]:
        for path in disclosed_path_strings(value, suffix):
            normalized = normalized.replace(path, normalize_output_bundle_path(path))
    return normalized


def normalize_text_value(value: str) -> str:
    return normalize_output_bundle_paths(normalize_busy_timeout_elapsed_ms(value))


def normalize_response(value: Any) -> Any:
    if isinstance(value, dict):
        return {key: normalize_response(item) for key, item in value.items()}
    if isinstance(value, list):
        return [normalize_response(item) for item in value]
    if isinstance(value, str):
        return normalize_text_value(value)
    return value


def r_string_literal(value: str) -> str:
    return json.dumps(value.replace("\\", "/"))


def r_large_text_input(label: str, lines: int = 80, width: int = 120) -> str:
    prefix = r_string_literal(f"{label}%03d %s\n")
    return (
        f"big <- paste(rep({r_string_literal(label)}, {width}), collapse = ''); "
        f"for (i in 1:{lines}) cat(sprintf({prefix}, i, big))"
    )


def repeated_text_line(label: str, index: int, width: int = 120) -> str:
    return f"{label}{index:03d} {label * width}\n"


def expected_large_text_preview(label: str, output_number: int) -> str:
    head = "".join(repeated_text_line(label, index) for index in range(1, 19))
    tail = "".join(repeated_text_line(label, index) for index in range(72, 81))
    return (
        head
        + "...[middle truncated; shown lines 1-18 and 72-81 of 81 total; "
        f"full output: <mcp-repl-output>/output-{output_number:04d}/transcript.txt]...\n"
        + tail
        + "> "
    )


def expected_capped_text_preview(label: str, output_number: int) -> str:
    lines = "".join(repeated_text_line(label, index) for index in range(1, 17))
    return (
        lines
        + f"{label}017 {label}\n"
        + f"...[full output: <mcp-repl-output>/output-{output_number:04d}/transcript.txt; "
        "later content omitted]..."
    )


def expected_timeout_bundle_poll_preview(output_number: int) -> str:
    head = "".join(
        f"timeout{index:03d} {'t' * 120}\n" for index in range(1, 18)
    )
    tail = "".join(
        f"timeout{index:03d} {'t' * 120}\n" for index in range(73, 81)
    )
    return (
        "TIMEOUT_BIG_START\n"
        '> for (i in 1:80) cat(sprintf("timeout%03d %s\\n", i, big))\n'
        + head
        + "...[middle truncated; shown lines 1-19 and 75-85 of 85 total; "
        f"full output: <mcp-repl-output>/output-{output_number:04d}/transcript.txt]...\n"
        + tail
        + '> cat("TIMEOUT_BIG_END\\n")\n'
        + "TIMEOUT_BIG_END\n"
        + "> "
    )


def expected_pager_lines(start: int, end: int) -> str:
    return "".join(f"L{index:04d}\n" for index in range(start, end + 1))


def require_transcript_path(text: str, context: str) -> Path:
    transcript_path = bundle_transcript_path(text)
    if transcript_path is None:
        raise SuiteFailure(f"{context} expected transcript.txt path, got: {text!r}")
    return transcript_path


def require_text_file(path: Path, context: str) -> str:
    if not path.is_file():
        raise SuiteFailure(f"{context} expected file to exist: {path}")
    return path.read_text(encoding="utf-8")


def r_console_basic(client: McpStdioClient) -> None:
    received = client.repl("1+1\n", timeout_ms=30000)

    expected = tool_result(
        text("[1] 2\n"),
        text("> "),
    )

    assert_identical(expected, received, "repl")


def r_timeout_busy_recovers(client: McpStdioClient) -> None:
    warmup = client.repl("1+1\n", timeout_ms=30000)
    assert_identical(
        tool_result(text("[1] 2\n"), text("> ")),
        warmup,
        "warmup repl",
    )

    timed_out = client.repl("Sys.sleep(1)\n", timeout_ms=300)
    assert_identical(
        tool_result(
            text("<<repl status: busy, write_stdin timeout reached; elapsed_ms=N>>")
        ),
        normalize_response(timed_out),
        "timeout repl",
    )

    busy_follow_up = client.repl("1+1\n", timeout_ms=300)
    assert_identical(
        tool_result(
            text("<<repl status: busy, write_stdin timeout reached; elapsed_ms=N>>"),
            text("[repl] input discarded while worker busy"),
        ),
        normalize_response(busy_follow_up),
        "busy follow-up repl",
    )

    wait_for_response(client, tool_result(text("> ")), "timeout poll repl")

    recovered = client.repl("1+1\n", timeout_ms=5000)
    assert_identical(
        tool_result(text("[1] 2\n"), text("> ")),
        recovered,
        "recovery repl",
    )


def r_reset_clears_state(client: McpStdioClient) -> None:
    set_var = client.repl("x <- 1\n", timeout_ms=30000)
    assert_identical(
        tool_result(text("> ")),
        set_var,
        "set variable repl",
    )

    reset = client.repl_reset()
    assert_identical(
        tool_result(text("[repl] new session started")),
        reset,
        "repl_reset",
    )

    after_reset = client.repl('print(exists("x"))\n', timeout_ms=30000)
    assert_identical(
        tool_result(text("[1] FALSE\n"), text("> ")),
        after_reset,
        "after reset repl",
    )


def r_interrupt_restart_prefixes(client: McpStdioClient) -> None:
    set_var = client.repl("x <- 1\n", timeout_ms=30000)
    assert_identical(
        tool_result(text("> ")),
        set_var,
        "set variable before restart",
    )

    restarted = client.repl('\u0004print(exists("x"))\n', timeout_ms=30000)
    assert_identical(
        tool_result(
            text("[repl] new session started\n"),
            text("[1] FALSE\n"),
            text("> "),
        ),
        restarted,
        "restart prefix repl",
    )

    long_running = r"""
cat("INTERRUPT_READY\n")
flush.console()
tryCatch(
  {
    repeat Sys.sleep(0.5)
  },
  interrupt = function(e) cat("interrupt received\n")
)
"""
    timed_out = client.repl(long_running, timeout_ms=1000)
    assert_identical(
        tool_result(
            text(
                '> \n'
                '> cat("INTERRUPT_READY\\n")\n'
                "INTERRUPT_READY\n"
                "> flush.console()\n"
                "> tryCatch(\n"
                "+   {\n"
                "+     repeat Sys.sleep(0.5)\n"
                "+   },\n"
                '+   interrupt = function(e) cat("interrupt received\\n")\n'
                "+ )\n"
            ),
            text("<<repl status: busy, write_stdin timeout reached; elapsed_ms=N>>"),
        ),
        normalize_response(timed_out),
        "interrupt setup repl",
    )

    interrupted = client.repl('\u0003cat("AFTER_INTERRUPT\\n")', timeout_ms=5000)
    assert_identical(
        tool_result(
            text("interrupt received\n"),
            text("AFTER_INTERRUPT\n"),
            text("> "),
        ),
        interrupted,
        "interrupt prefix repl",
    )


def r_output_bundle_files(client: McpStdioClient) -> None:
    oversized = client.repl(r_large_text_input("x"), timeout_ms=30000)
    assert_identical(
        tool_result(text(expected_large_text_preview("x", 1))),
        normalize_response(oversized),
        "output bundle text repl",
    )
    oversized_text = result_text(oversized)
    transcript_path = require_transcript_path(
        oversized_text,
        "output bundle text repl",
    )
    transcript = require_text_file(transcript_path, "output bundle text transcript")
    if "x080" not in transcript:
        raise SuiteFailure(
            f"expected transcript bundle to contain full worker text, got: {transcript!r}"
        )
    bundle_dir = transcript_path.parent
    if (bundle_dir / "events.log").exists():
        raise SuiteFailure("did not expect events.log for text-only output bundle")
    if (bundle_dir / "images").exists():
        raise SuiteFailure("did not expect images dir for text-only output bundle")

    bundle_paths: list[Path] = []
    for output_number, label in enumerate(["a", "b", "c"], start=2):
        received = client.repl(r_large_text_input(label), timeout_ms=30000)
        assert_identical(
            tool_result(text(expected_large_text_preview(label, output_number))),
            normalize_response(received),
            f"output bundle pruning repl {label}",
        )
        bundle_paths.append(
            require_transcript_path(
                result_text(received),
                f"output bundle pruning repl {label}",
            )
        )

    if bundle_paths[0].exists():
        raise SuiteFailure(
            f"expected oldest inactive bundle to be pruned: {bundle_paths[0]}"
        )
    if not bundle_paths[1].exists() or not bundle_paths[2].exists():
        raise SuiteFailure(f"expected newest bundles to remain: {bundle_paths!r}")

    with tempfile.TemporaryDirectory() as temp_dir:
        gate = Path(temp_dir) / "release"
        input_text = f"""
cat("TIMEOUT_START\\n")
flush.console()
while (!file.exists({r_string_literal(str(gate))})) Sys.sleep(0.05)
big <- paste(rep("t", 120), collapse = "")
cat("TIMEOUT_BIG_START\\n")
for (i in 1:80) cat(sprintf("timeout%03d %s\\n", i, big))
cat("TIMEOUT_BIG_END\\n")
"""
        first = client.repl(input_text, timeout_ms=1000)
        assert_identical(
            tool_result(
                text(
                    "TIMEOUT_START\n"
                    "> flush.console()\n"
                    f"> while (!file.exists({r_string_literal(str(gate))})) Sys.sleep(0.05)\n"
                ),
                text("<<repl status: busy, write_stdin timeout reached; elapsed_ms=N>>"),
            ),
            normalize_response(first),
            "output bundle timeout setup repl",
        )
        first_text = result_text(first)
        if bundle_transcript_path(first_text) is not None:
            raise SuiteFailure(
                f"did not expect timeout setup to disclose a bundle, got: {first_text!r}"
            )

        gate.write_text("ready", encoding="utf-8")
        settled = client.repl("", timeout_ms=10000)
        assert_identical(
            tool_result(text(expected_timeout_bundle_poll_preview(5))),
            normalize_response(settled),
            "output bundle timeout poll repl",
        )
        settled_text = result_text(settled)
        timeout_transcript_path = require_transcript_path(
            settled_text,
            "output bundle timeout poll repl",
        )
        timeout_transcript = require_text_file(
            timeout_transcript_path,
            "output bundle timeout transcript",
        )
        if "TIMEOUT_START" not in timeout_transcript:
            raise SuiteFailure(
                "expected timeout transcript to backfill earlier worker text, "
                f"got: {timeout_transcript!r}"
            )
        if "TIMEOUT_BIG_END" not in timeout_transcript:
            raise SuiteFailure(
                f"expected timeout transcript to include later worker text, got: {timeout_transcript!r}"
            )
        if "<<repl status: busy" in timeout_transcript:
            raise SuiteFailure(
                f"did not expect timeout marker in transcript, got: {timeout_transcript!r}"
            )


def r_output_bundle_size_limit(client: McpStdioClient) -> None:
    received = client.repl(r_large_text_input("z", lines=120), timeout_ms=30000)
    assert_identical(
        tool_result(text(expected_capped_text_preview("z", 1))),
        normalize_response(received),
        "output bundle size limit repl",
    )
    received_text = result_text(received)
    transcript_path = require_transcript_path(
        received_text,
        "output bundle size limit repl",
    )
    transcript = require_text_file(
        transcript_path,
        "output bundle size limit transcript",
    )
    if (transcript_path.parent / "events.log").exists():
        raise SuiteFailure("did not expect events.log for text-only capped bundle")
    if "z120" in transcript:
        raise SuiteFailure(
            f"did not expect capped transcript to contain omitted tail, got: {transcript!r}"
        )


def r_pager_command_smoke(client: McpStdioClient) -> None:
    initial = client.repl(
        'for (i in 1:80) cat(sprintf("L%04d\\n", i))\n',
        timeout_ms=120000,
    )
    assert_identical(
        tool_result(
            text(expected_pager_lines(1, 13)),
            text("--More-- (6p, 16.2%, @0..78/480)"),
        ),
        initial,
        "pager initial repl",
    )

    next_page = client.repl(":next", timeout_ms=60000)
    assert_identical(
        tool_result(
            text(expected_pager_lines(14, 26)),
            text("--More-- (5p, 32.5%, @78..156/480)"),
        ),
        next_page,
        "pager next repl",
    )

    search = client.repl(":/L0031", timeout_ms=60000)
    assert_identical(
        tool_result(
            text("[pager] search for `L0031` @180"),
            text("[match] L0031\n"),
            text("--More-- (4p, 37.5%, @180/480)"),
        ),
        search,
        "pager search repl",
    )

    quit_result = client.repl(":q", timeout_ms=60000)
    assert_identical(
        tool_result(
            text("(END, 37.5%, @180/480)"),
            text("> "),
        ),
        quit_result,
        "pager quit repl",
    )


@dataclass(frozen=True)
class SuiteCase:
    run: Callable[[McpStdioClient], None]
    server_args: tuple[str, ...] = ()
    server_env: tuple[tuple[str, str], ...] = ()


def r_suite_case(
    run: Callable[[McpStdioClient], None],
    *,
    server_args: tuple[str, ...] = (),
    server_env: tuple[tuple[str, str], ...] = (),
) -> SuiteCase:
    return SuiteCase(
        run,
        server_args=("--interpreter", "r", *server_args),
        server_env=server_env,
    )


CASES: dict[str, SuiteCase] = {
    "r-console-basic": r_suite_case(r_console_basic),
    "r-interrupt-restart-prefixes": r_suite_case(r_interrupt_restart_prefixes),
    "r-output-bundle-files": r_suite_case(
        r_output_bundle_files,
        server_args=("--oversized-output", "files"),
        server_env=(
            ("MCP_REPL_OUTPUT_BUNDLE_MAX_COUNT", "2"),
            ("MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES", "1048576"),
            ("MCP_REPL_OUTPUT_BUNDLE_MAX_TOTAL_BYTES", "2097152"),
        ),
    ),
    "r-output-bundle-size-limit": r_suite_case(
        r_output_bundle_size_limit,
        server_args=("--oversized-output", "files"),
        server_env=(
            ("MCP_REPL_OUTPUT_BUNDLE_MAX_COUNT", "20"),
            ("MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES", "2048"),
            ("MCP_REPL_OUTPUT_BUNDLE_MAX_TOTAL_BYTES", "1048576"),
        ),
    ),
    "r-pager-command-smoke": r_suite_case(
        r_pager_command_smoke,
        server_args=("--oversized-output", "pager"),
        server_env=(("MCP_REPL_PAGER_PAGE_CHARS", "80"),),
    ),
    "r-reset-clears-state": r_suite_case(r_reset_clears_state),
    "r-timeout-busy-recovers": r_suite_case(r_timeout_busy_recovers),
}


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run public MCP API tests against a built mcp-repl binary."
    )
    parser.add_argument(
        "--binary",
        required=True,
        type=Path,
        help="path to an already-built mcp-repl binary",
    )
    parser.add_argument(
        "--case",
        action="append",
        choices=sorted(CASES),
        help="case to run; repeat to select multiple cases",
    )
    parser.add_argument(
        "--sandbox",
        default="danger-full-access",
        help="sandbox mode to pass to mcp-repl",
    )
    parser.add_argument(
        "--timeout",
        default=DEFAULT_TIMEOUT_SECONDS,
        type=float,
        help="per-request timeout in seconds",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str]) -> int:
    args = parse_args(argv)
    if args.timeout <= 0:
        print("--timeout must be positive", file=sys.stderr)
        return 2

    selected = args.case or sorted(CASES)
    failures = 0
    for case_name in selected:
        case = CASES[case_name]
        try:
            with McpStdioClient(
                args.binary,
                ["--sandbox", args.sandbox, *case.server_args],
                case.server_env,
                args.timeout,
            ) as client:
                case.run(client)
        except SuiteFailure as exc:
            failures += 1
            print(f"not ok {case_name}: {exc}", file=sys.stderr)
        else:
            print(f"ok {case_name}")

    if failures:
        print(f"{failures} failed", file=sys.stderr)
        return 1
    print(f"{len(selected)} passed")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
