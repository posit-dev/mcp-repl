#!/usr/bin/env -S uv run --script

import json
import os
import select
import subprocess
import sys
import tempfile
import time
from collections.abc import Iterator
from contextlib import closing, contextmanager
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from support.requirements import POSIX, R, command, requires
from support.checkpoints import FifoCheckpoint
from support.assertions import last_result_text
from support.progress import elapsed_progress, without_elapsed_result
from support.client import McpClient
from support.execution import DIRECT, Execution, executions
from support.normalization import code, normalize_process_diagnostic
from support.records import Transcript
from support.r import r_test_environment
from support.discovery import DiscoveryGate, discovery_environment
from support.suites import run_this_suite


@contextmanager
def gated_discovery(
    binary: Path,
    *,
    r_home: Path | None = None,
) -> Iterator[tuple[McpClient, DiscoveryGate]]:
    with discovery_environment(r_home=r_home) as (environment, discovery):
        with McpClient(
            binary, ("serve", "--no-sandbox"), environment, response_timeout=5
        ) as client:
            discovery.wait("runtime discovery is blocked")
            yield client, discovery


def closed_input_with_blocked_stdout(
    binary: Path, *, discover_first: bool
) -> Transcript:
    with discovery_environment() as (environment, discovery):
        read_output, write_output = os.pipe()
        try:
            with tempfile.TemporaryDirectory() as directory:
                environment["MCP_CONSOLE_HOME"] = directory
                process = subprocess.Popen(
                    [binary, "serve", "--no-sandbox"],
                    env=environment,
                    cwd=directory,
                    stdin=subprocess.PIPE,
                    stdout=write_output,
                    stderr=subprocess.PIPE,
                )
                assert process.stdin is not None and process.stderr is not None
                try:
                    discovery.wait("runtime discovery is blocked")
                    if discover_first:
                        process.stdin.write(
                            json.dumps(
                                {
                                    "jsonrpc": "2.0",
                                    "id": 0,
                                    "method": "server/discover",
                                    "params": {
                                        "_meta": {
                                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                            "io.modelcontextprotocol/clientInfo": {
                                                "name": "startup-test",
                                                "version": "1",
                                            },
                                            "io.modelcontextprotocol/clientCapabilities": {},
                                        }
                                    },
                                }
                            ).encode()
                            + b"\n"
                        )
                        process.stdin.flush()
                        response = b""
                        while not response.endswith(b"\n"):
                            assert select.select([read_output], [], [], 5)[0], (
                                "bootstrap discovery did not respond"
                            )
                            chunk = os.read(read_output, 4096)
                            assert chunk, "bootstrap discovery closed its output"
                            response += chunk
                        assert (
                            json.loads(response)["result"]["resultType"] == "complete"
                        )
                    os.set_blocking(write_output, False)
                    try:
                        while True:
                            os.write(write_output, b"x" * 4096)
                    except BlockingIOError:
                        pass
                    os.set_blocking(write_output, True)
                    process.stdin.write(
                        json.dumps(
                            {
                                "jsonrpc": "2.0",
                                "id": 1,
                                "method": "initialize",
                                "params": {
                                    "protocolVersion": "2025-03-26",
                                    "capabilities": {},
                                    "clientInfo": {
                                        "name": "startup-test",
                                        "version": "1",
                                    },
                                },
                            }
                        ).encode()
                        + b"\n"
                    )
                    process.stdin.close()
                    assert process.wait(timeout=5) != 0
                    assert (
                        b"server startup cancelled because MCP input closed"
                        in process.stderr.read()
                    )
                finally:
                    if process.poll() is None:
                        discovery.release()
                        process.kill()
                        process.wait(timeout=5)
                    process.stderr.close()
        finally:
            os.close(write_output)
            os.close(read_output)
    return [{"closed_input_retired_discovery_with_blocked_stdout": True}]


@requires(POSIX, command("rustc"))
def test_closed_input_cancels_discovery_with_blocked_stdout(binary: Path) -> Transcript:
    return closed_input_with_blocked_stdout(binary, discover_first=False)


@requires(POSIX, command("rustc"))
def test_completed_discovery_does_not_hide_cancelled_initialization(
    binary: Path,
) -> Transcript:
    return closed_input_with_blocked_stdout(binary, discover_first=True)


@requires(command("rustc"))
def test_closed_input_before_handshake_reports_cancelled_preparation(
    binary: Path,
) -> Transcript:
    with gated_discovery(binary) as (client, _):
        client.stdin.close()
        assert client.process.wait(timeout=5) != 0
        assert client.stdout.read() == ""
        errors = client.stderr.read()
        assert errors == "server startup cancelled because MCP input closed\n", errors
        return [{"stderr": errors}]


@requires(command("rustc"))
def test_initializes_while_runtime_discovery_is_blocked(binary: Path) -> Transcript:
    with gated_discovery(binary) as (client, _):
        client.initialize_and_list_tools()
        client.request("ping")
        return client.finish()


@requires(command("rustc"))
def test_reports_discovery_failure_without_losing_mcp(binary: Path) -> Transcript:
    with gated_discovery(binary) as (client, release):
        client.initialize_and_list_tools()
        pending = client.start_send(r="stop('must not execute')")
        ping = client.start_request("ping")
        client.receive(ping)
        release.release()
        client.receive(pending)
        assert "fixture R discovery failed" in str(pending), pending
        assert pending["result"]["isError"] is True, pending
        assert client.send(r="stop('must not retry discovery')") == pending["result"]
        inspected = client.send(requirements={"action": "get"})
        assert inspected["isError"] is True, inspected
        assert "fixture R discovery failed" in str(inspected), inspected
        assert client.request("tools/list")["result"] == client.transcript[2]["result"]
        transcript, errors = client.finish_with_standard_error(expected_exit_status=1)
        assert "fixture R discovery failed" in errors, errors
        return json.loads(
            normalize_process_diagnostic(json.dumps(transcript + [{"stderr": errors}]))
        )


@requires(POSIX, command("rustc"))
def test_standalone_requirements_wait_for_discovery(binary: Path) -> Transcript:
    with gated_discovery(binary) as (client, release):
        return standalone_requirements_during_discovery(client, release)


def standalone_requirements_during_discovery(
    client: McpClient, release: DiscoveryGate
) -> Transcript:
    client.transcript.append(
        {
            "scenario": "Standalone preparation waits for initial discovery even with timeout_ms=0.",
            "fixture": "The R discovery probe is held at a loopback socket and fails after release. A ping sent after the request acknowledges readiness-wait dispatch.",
        }
    )
    client.initialize_and_list_tools()
    pending = client.start_send(
        requirements={"action": "set", "r": ["must-not-resolve"]},
        timeout_ms=0,
    )
    # The send handler enters the current-thread runtime's FIFO ready queue at
    # receipt, before the later ping. It runs through validation to its
    # readiness wait before that ping can reply.
    ping = client.start_request("ping")
    client.receive(ping)
    assert ping["result"] == {}, ping
    if select.select([client.stdout], [], [], 0)[0]:
        client.receive(pending)
        raise AssertionError(
            f"standalone preparation returned during discovery: {pending}"
        )
    release.release()
    client.receive(pending)
    assert pending["result"]["isError"] is True, pending
    assert "fixture R discovery failed" in pending["result"]["content"][0]["text"], (
        pending
    )
    transcript, errors = client.finish_with_standard_error(expected_exit_status=1)
    assert "fixture R discovery failed" in errors, errors
    return transcript + [{"stderr": errors}]


@requires(command("rustc"))
def test_failed_handshake_cancels_discovery_with_input_open(binary: Path) -> Transcript:
    with gated_discovery(binary) as (client, _):
        # Requests without inline metadata now receive recoverable errors.
        # An initialized notification before initialize is a fatal handshake error.
        client.notify("notifications/initialized")
        assert client.process.wait(timeout=5) != 0
        errors = client.stderr.read()
        assert errors, "invalid handshake must report its failure"
        return [{"stderr": errors, "input_remained_open": not client.stdin.closed}]


@requires(command("rustc"))
def test_bounds_discovery_failure_during_requirement_inspection(
    binary: Path,
) -> Transcript:
    with discovery_environment() as (environment, discovery):
        (discovery.root / "failure").write_text(
            "fixture R discovery failed " + "x" * (32 * 1024)
        )
        with McpClient(binary, ("serve", "--no-sandbox"), environment) as client:
            discovery.wait("runtime discovery is blocked")
            client.initialize_and_list_tools()
            discovery.release()
            failure = client.send(requirements={"action": "get"})
            assert failure["isError"] is True, failure
            content = failure["content"][0]["text"]
            assert "fixture R discovery failed" in content, failure
            assert len(content.encode()) <= 8192, len(content.encode())
            _, stderr = client.finish_with_standard_error(expected_exit_status=1)
            assert "fixture R discovery failed" in stderr
            return [{"inspection_error": "bounded to 8 KiB", "isError": True}]


@requires(command("rustc"))
def test_cancelled_send_does_not_cancel_shared_discovery(binary: Path) -> Transcript:
    with gated_discovery(binary) as (client, release):
        client.initialize_and_list_tools()
        pending = client.start_send(r="stop('cancelled request must not execute')")
        wait_for_send_admission(client)
        client.send(r="stop('another cell must not execute')", timeout_ms=0)
        assert "already evaluating" in str(client.transcript[-1]["result"])
        client.notify("notifications/cancelled", requestId=pending["id"])
        client.request("ping")
        release.release()
        result = client.send(session="default")
        assert result["isError"], result
        assert "fixture R discovery failed" in str(result), result
        transcript, errors = client.finish_with_standard_error(expected_exit_status=1)
        assert "fixture R discovery failed" in errors, errors
        return json.loads(
            normalize_process_diagnostic(json.dumps(transcript + [{"stderr": errors}]))
        )


def wait_for_send_admission(client: McpClient, *, session: str = "default") -> None:
    # Writing a request does not mean its handler has reserved the cell yet.
    # Startup is gated, so an empty observation can only report startup or
    # the pending send's exclusive wait claim. Never submit a competing cell
    # until that public receipt proves admission.
    deadline = time.monotonic() + 30
    first_poll = len(client.transcript)
    while True:
        result = client.send(session=session, timeout_ms=0)
        if result.get("isError"):
            assert result == {
                "content": [
                    {
                        "type": "text",
                        "text": "[worker evaluation is already being polled]",
                    }
                ],
                "isError": True,
            }, result
            break
        assert result["content"] in (
            [{"type": "text", "text": "[worker starting]"}],
            [{"type": "text", "text": "\n[phase: startup]\n[worker starting]"}],
            [
                {
                    "type": "text",
                    "text": "\n[phase: dependency preparation]\n[worker starting]",
                }
            ],
        ), result
        assert time.monotonic() < deadline, "pending send did not claim its evaluation"
    # Only the scheduling-dependent startup observations are incidental.
    client.transcript[first_poll:] = [client.transcript[-1]]


@requires(POSIX, command("rustc"))
@requires(R)
def test_queued_r_cell_executes_once_after_discovery(binary: Path) -> Transcript:
    environment, _ = r_test_environment()
    with (
        gated_discovery(binary, r_home=Path(environment["R_HOME"])) as (
            client,
            release,
        ),
        closing(
            FifoCheckpoint.create(Path(client.temporary_directory.name) / "evaluated")
        ) as evaluated,
    ):
        client.initialize_and_list_tools()
        pending = client.start_send(
            # fmt: r
            r=code("""
                started <- get0("started", ifnotfound = 0L) + 1L
                checkpoint <- fifo("evaluated", "wb", blocking = TRUE)
                writeBin(charToRaw("1"), checkpoint)
                close(checkpoint)
                started
                """),
            timeout_ms=0,
        )
        client.receive(pending)
        assert without_elapsed_result(pending["result"])["content"] == [
            {"type": "text", "text": "\n[running; poll with an empty send]"}
        ], pending
        client.request("ping")
        release.release()
        evaluated.wait("the accepted R cell executed", timeout=600)
        client.send(session="default", timeout_ms=10_000)
        assert last_result_text(client) == "[1] 1\n", client.transcript[-1]
        client.send(r="started")
        assert last_result_text(client) == "[1] 1\n", last_result_text(client)
        return client.finish()


@requires(command("rustc"))
@requires(R)
def test_cancelled_wait_preserves_admitted_cell_after_discovery(
    binary: Path,
) -> Transcript:
    environment, _ = r_test_environment()
    with gated_discovery(binary, r_home=Path(environment["R_HOME"])) as (
        client,
        release,
    ):
        client.initialize_and_list_tools()
        pending = client.start_send(r="cancelled_cell_ran <- TRUE")
        wait_for_send_admission(client)
        client.send(r="stop('another cell must not execute')", timeout_ms=0)
        assert "already evaluating" in str(client.transcript[-1]["result"])
        client.notify("notifications/cancelled", requestId=pending["id"])
        client.request("ping")
        client.send(session="default", timeout_ms=250)
        age, silent = elapsed_progress(last_result_text(client))
        assert age >= 0.2 and silent
        release.release()
        # Releasing discovery still leaves cold R startup and the admitted
        # cell to finish. Keep the raw cancellation/poll exchange intact.
        client.response_timeout = 600
        try:
            client.send(session="default", timeout_ms=600_000)
        finally:
            client.response_timeout = 5
        assert last_result_text(client) == "[done]", client.transcript[-1]
        client.send(r='exists("cancelled_cell_ran", inherits = FALSE)')
        assert last_result_text(client) == "[1] TRUE\n", last_result_text(client)
        return client.finish()


@executions(DIRECT)
def test_first_send_uses_background_runtime(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        environment = os.environ.copy()
        environment.pop("R_HOME", None)
        environment["PATH"] = temporary
        with McpClient(
            binary,
            execution.serve("-c", "python=" + json.dumps(sys.executable)),
            environment,
        ) as client:
            client.initialize_and_list_tools()
            client.send(python="started = globals().get('started', 0) + 1\nstarted")
            assert last_result_text(client) == "1\n", last_result_text(client)
            client.send(python="started")
            assert last_result_text(client) == "1\n", last_result_text(client)
            return client.finish()


if __name__ == "__main__":
    run_this_suite(__file__)
