#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import queue
import re
import subprocess
import sys
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


def result_text(result: dict[str, Any]) -> str:
    content = result.get("content")
    if not isinstance(content, list):
        raise SuiteFailure(f"tool result content is not a list: {result!r}")
    chunks: list[str] = []
    for item in content:
        if isinstance(item, dict) and isinstance(item.get("text"), str):
            chunks.append(item["text"])
    return "".join(chunks)


def require_success(result: dict[str, Any], context: str) -> str:
    text = result_text(result)
    if result.get("isError") is True:
        raise SuiteFailure(f"{context} returned isError=true: {text!r}")
    return text


def require_r_result_two(text: str, context: str) -> None:
    if re.search(r"(?m)(^|\s)2(\s|$)", text) is None:
        raise SuiteFailure(f"{context} expected R console result 2, got: {text!r}")


def is_busy_response(text: str) -> bool:
    return (
        "<<repl status: busy" in text
        or "worker is busy" in text
        or "request already running" in text
        or "input discarded while worker busy" in text
    )


def wait_until_not_busy(
    client: McpStdioClient,
    context: str,
    deadline_seconds: float = 5.0,
) -> str:
    assert deadline_seconds > 0
    deadline = time.monotonic() + deadline_seconds
    last_text = ""
    while time.monotonic() < deadline:
        result = client.call_tool(
            "repl",
            {
                "input": "",
                "timeout_ms": 500,
            },
        )
        last_text = require_success(result, context)
        if not is_busy_response(last_text):
            return last_text
    raise SuiteFailure(f"{context} remained busy after polling: {last_text!r}")


def require_success_when_not_busy(
    client: McpStdioClient,
    result: dict[str, Any],
    context: str,
) -> str:
    text = require_success(result, context)
    if not is_busy_response(text):
        return text
    return wait_until_not_busy(client, context)


def wait_for_interrupt_ready(client: McpStdioClient, text: str, context: str) -> str:
    deadline = time.monotonic() + 20.0
    last_text = text
    while time.monotonic() < deadline:
        if "INTERRUPT_READY" in last_text:
            return last_text
        if not is_busy_response(last_text):
            raise SuiteFailure(
                f"{context} finished before interrupt readiness marker: {last_text!r}"
            )
        result = client.call_tool(
            "repl",
            {
                "input": "",
                "timeout_ms": 500,
            },
        )
        last_text = require_success(result, context)
    raise SuiteFailure(f"{context} did not reach interrupt readiness: {last_text!r}")


def r_console_basic(client: McpStdioClient) -> None:
    result = client.call_tool(
        "repl",
        {
            "input": "1+1\n",
            "timeout_ms": 30000,
        },
    )
    text = require_success(result, "repl")
    require_r_result_two(text, "repl")


def r_timeout_busy_recovers(client: McpStdioClient) -> None:
    warmup = client.call_tool(
        "repl",
        {
            "input": "1+1\n",
            "timeout_ms": 30000,
        },
    )
    require_r_result_two(require_success(warmup, "warmup repl"), "warmup repl")

    timed_out = client.call_tool(
        "repl",
        {
            "input": "Sys.sleep(1)\n",
            "timeout_ms": 300,
        },
    )
    timed_out_text = require_success(timed_out, "timeout repl")
    if "<<repl status: busy" not in timed_out_text:
        raise SuiteFailure(f"expected timeout busy marker, got: {timed_out_text!r}")

    busy_follow_up = client.call_tool(
        "repl",
        {
            "input": "1+1\n",
            "timeout_ms": 300,
        },
    )
    busy_text = require_success(busy_follow_up, "busy follow-up repl")
    if "<<repl status: busy" not in busy_text:
        raise SuiteFailure(f"expected busy follow-up marker, got: {busy_text!r}")
    if "input discarded while worker busy" not in busy_text:
        raise SuiteFailure(f"expected busy input discard notice, got: {busy_text!r}")

    wait_until_not_busy(client, "timeout poll repl")

    recovered = client.call_tool(
        "repl",
        {
            "input": "1+1\n",
            "timeout_ms": 5000,
        },
    )
    recovered_text = require_success(recovered, "recovery repl")
    if "<<repl status: busy" in recovered_text:
        raise SuiteFailure(f"expected recovery response, got: {recovered_text!r}")
    require_r_result_two(recovered_text, "recovery repl")


def r_reset_clears_state(client: McpStdioClient) -> None:
    set_var = client.call_tool(
        "repl",
        {
            "input": "x <- 1\n",
            "timeout_ms": 30000,
        },
    )
    set_var_text = require_success(set_var, "set variable repl")
    if "<<repl status: busy" in set_var_text:
        raise SuiteFailure(f"expected set variable response, got: {set_var_text!r}")

    reset = client.call_tool("repl_reset", {})
    require_success(reset, "repl_reset")

    after_reset = client.call_tool(
        "repl",
        {
            "input": "print(exists(\"x\"))\n",
            "timeout_ms": 30000,
        },
    )
    after_reset_text = require_success(after_reset, "after reset repl")
    if "<<repl status: busy" in after_reset_text:
        raise SuiteFailure(f"expected after-reset response, got: {after_reset_text!r}")
    if "FALSE" not in after_reset_text:
        raise SuiteFailure(f"expected reset state, got: {after_reset_text!r}")


def r_interrupt_restart_prefixes(client: McpStdioClient) -> None:
    set_var = client.call_tool(
        "repl",
        {
            "input": "x <- 1\n",
            "timeout_ms": 30000,
        },
    )
    require_success_when_not_busy(client, set_var, "set variable before restart")

    restarted = client.call_tool(
        "repl",
        {
            "input": "\u0004print(exists(\"x\"))\n",
            "timeout_ms": 30000,
        },
    )
    restarted_text = require_success_when_not_busy(
        client,
        restarted,
        "restart prefix repl",
    )
    if "FALSE" not in restarted_text:
        raise SuiteFailure(f"expected restart prefix to clear state, got: {restarted_text!r}")

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
    timed_out = client.call_tool(
        "repl",
        {
            "input": long_running,
            "timeout_ms": 300,
        },
    )
    timed_out_text = require_success(timed_out, "interrupt setup repl")
    wait_for_interrupt_ready(client, timed_out_text, "interrupt setup repl")

    interrupted = client.call_tool(
        "repl",
        {
            "input": "\u0003cat(\"AFTER_INTERRUPT\\n\")",
            "timeout_ms": 5000,
        },
    )
    interrupted_text = require_success_when_not_busy(
        client,
        interrupted,
        "interrupt prefix repl",
    )
    if "interrupt received" not in interrupted_text:
        raise SuiteFailure(
            f"expected interrupt handler output, got: {interrupted_text!r}"
        )
    if "AFTER_INTERRUPT" not in interrupted_text:
        raise SuiteFailure(
            f"expected remaining input after interrupt, got: {interrupted_text!r}"
        )


def r_pager_command_smoke(client: McpStdioClient) -> None:
    initial = client.call_tool(
        "repl",
        {
            "input": "for (i in 1:80) cat(sprintf(\"L%04d\\n\", i))\n",
            "timeout_ms": 120000,
        },
    )
    initial_text = require_success_when_not_busy(client, initial, "pager initial repl")
    if "L0001" not in initial_text:
        raise SuiteFailure(f"expected first pager page, got: {initial_text!r}")
    if "--More--" not in initial_text:
        raise SuiteFailure(f"expected pager footer, got: {initial_text!r}")

    next_page = client.call_tool(
        "repl",
        {
            "input": ":next",
            "timeout_ms": 60000,
        },
    )
    next_text = require_success_when_not_busy(client, next_page, "pager next repl")
    if not any(needle in next_text for needle in ["L0002", "L0003", "L0010", "L0014"]):
        raise SuiteFailure(f"expected next pager page, got: {next_text!r}")

    search = client.call_tool(
        "repl",
        {
            "input": ":/L0031",
            "timeout_ms": 60000,
        },
    )
    search_text = require_success_when_not_busy(client, search, "pager search repl")
    if "L0031" not in search_text and "next match" not in search_text:
        raise SuiteFailure(f"expected pager search guidance/output, got: {search_text!r}")

    quit_result = client.call_tool(
        "repl",
        {
            "input": ":q",
            "timeout_ms": 60000,
        },
    )
    require_success_when_not_busy(client, quit_result, "pager quit repl")


@dataclass(frozen=True)
class SuiteCase:
    run: Callable[[McpStdioClient], None]
    server_args: tuple[str, ...] = ()
    server_env: tuple[tuple[str, str], ...] = ()


CASES: dict[str, SuiteCase] = {
    "r-console-basic": SuiteCase(r_console_basic),
    "r-interrupt-restart-prefixes": SuiteCase(r_interrupt_restart_prefixes),
    "r-pager-command-smoke": SuiteCase(
        r_pager_command_smoke,
        server_args=("--oversized-output", "pager"),
        server_env=(("MCP_REPL_PAGER_PAGE_CHARS", "80"),),
    ),
    "r-reset-clears-state": SuiteCase(r_reset_clears_state),
    "r-timeout-busy-recovers": SuiteCase(r_timeout_busy_recovers),
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
