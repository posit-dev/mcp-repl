#!/usr/bin/env -S uv run --script

import json
import sys
import tempfile
import time
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.python.test_startup import selected_python
from boundaries.client_server.python.test_without_r import environment as without_r
from boundaries.client_server.recording.test_boundaries import await_event
from support.assertions import last_result_text
from support.checkpoints import FifoCheckpoint
from support.client import McpClient
from support.execution import DIRECT, RUNTIME, SANDBOXED, Execution, executions
from support.normalization import code
from support.native import LOADER_VARIABLE, build_interposer
from support.progress import RUNNING, without_elapsed_result
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, POSIX, PROCESS_EVENTS, requires
from support.suites import run_this_suite


@executions(RUNTIME)
def test_named_sessions_keep_separate_state(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with McpClient(
            binary,
            execution.serve(),
            selected_python(root, Path(sys.executable)),
            root,
        ) as client:
            client.initialize_and_list_tools()
            properties = client.transcript[-1]["result"]["tools"][0]["inputSchema"][
                "properties"
            ]
            assert properties["session"]["default"] == "default", properties
            client.expect(python="value = 'default state'")
            client.expect(python="value = 'alpha state'", session="alpha")
            client.expect(python="value = 'beta state'", session="beta")
            client.expect("'alpha state'\n", python="value", session="alpha")
            client.expect("'beta state'\n", python="value", session="beta")
            client.expect("'default state'\n", python="value")
            client.expect("'default state'\n", python="value", session="default")
            return client.finish()


@executions(RUNTIME)
def test_session_arguments_have_no_effect_until_valid_code(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with McpClient(
            binary,
            execution.serve(),
            selected_python(root, Path(sys.executable)),
            root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(python="retained = 'default state'")
            baseline = {
                "content": [{"type": "text", "text": "\n[idle]"}],
                "isError": False,
            }
            assert client.send(timeout_ms=0) == baseline
            for arguments in (
                {"session": "unknown", "timeout_ms": 0},
                {"session": None, "python": "retained = 'invalid null'"},
                {"session": 42, "python": "retained = 'invalid number'"},
                {
                    "session": "invalid",
                    "python": "retained = 'invalid source'",
                    "r": "42",
                },
            ):
                rejected = client.send(**arguments)
                assert rejected["isError"] is True, rejected
                assert client.send(timeout_ms=0) == baseline
            client.expect("'default state'\n", python="retained")
            client.expect(python="retained = 'empty session name'", session="")
            client.expect("'empty session name'\n", python="retained", session="")
            assert client.send(timeout_ms=0) == {
                "content": [
                    {
                        "type": "text",
                        "text": '[session: ""; status: idle]\n\n[idle]\n'
                        '[session: "default"; status: idle]\n\n[idle]',
                    }
                ],
                "isError": False,
            }
            return client.finish()


@executions(RUNTIME)
def test_named_sessions_can_wait_for_input_concurrently(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with McpClient(
            binary,
            execution.serve(),
            selected_python(root, Path(sys.executable)),
            root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(python="default_value = 42")
            client.expect(
                '[input requested: "alpha> "]\n[waiting for stdin]',
                python="answer = input('alpha> ')",
                session="alpha",
            )
            client.expect(
                '[input requested: "beta> "]\n[waiting for stdin]',
                python="answer = input('beta> ')",
                session="beta",
            )
            active = client.send(timeout_ms=0)
            assert active == {
                "content": [
                    {
                        "type": "text",
                        "text": '[session: "alpha"; status: waiting_for_stdin]\n\n[waiting for stdin]\n'
                        '[session: "beta"; status: waiting_for_stdin]\n\n[waiting for stdin]\n'
                        '[session: "default"; status: idle]\n\n[idle]',
                    }
                ],
                "isError": False,
            }, active
            null_note = client.send(note=None, timeout_ms=0)
            assert null_note == active, null_note
            client.send(session="alpha", timeout_ms=0)
            assert last_result_text(client) == "\n[waiting for stdin]"
            client.expect(stdin="beta answer\n", session="beta")
            client.expect("'beta answer'\n", python="answer", session="beta")
            client.send(session="alpha", timeout_ms=0)
            assert last_result_text(client) == "\n[waiting for stdin]"
            client.expect(stdin="alpha answer\n", session="alpha")
            client.expect("'alpha answer'\n", python="answer", session="alpha")
            active = client.send(timeout_ms=0)
            assert active == {
                "content": [
                    {
                        "type": "text",
                        "text": '[session: "alpha"; status: idle]\n\n[idle]\n'
                        '[session: "beta"; status: idle]\n\n[idle]\n'
                        '[session: "default"; status: idle]\n\n[idle]',
                    }
                ],
                "isError": False,
            }, active
            return client.finish()


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_named_session_completes_while_another_send_waits(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary).resolve()
        with (
            closing(FifoCheckpoint.create(root / "alpha-ready")) as ready,
            closing(FifoCheckpoint.create(root / "alpha-release")) as release,
            McpClient(
                binary,
                execution.serve(
                    *(("--writable-root", str(root)) if execution == SANDBOXED else ())
                ),
                selected_python(root, Path(sys.executable)),
                root,
            ) as client,
        ):
            client.initialize_and_list_tools()
            for session in ("alpha", "beta"):
                client.expect(python="pass", session=session)
            # The warmup frame can arrive before its delivery settles. Do not
            # enqueue alpha and beta behind that unsettled response write.
            assert client.request("ping")["result"] == {}
            try:
                alpha = client.start_send(
                    session="alpha",
                    timeout_ms=300_000,
                    # fmt: python
                    python=code("""
                        with open("alpha-ready", "wb", buffering=0) as ready:
                            _ = ready.write(b"1")
                        with open("alpha-release", "rb", buffering=0) as release:
                            assert release.read(1) == b"1"
                        print("alpha returned")
                        """),
                )
                ready.wait("alpha is executing while its send waits", client=client)
                # Receiving beta before opening alpha's gate proves independent
                # admission and response delivery while alpha's call is pending.
                client.expect(
                    "beta returned\n",
                    python="print('beta returned')",
                    session="beta",
                )
                assert "result" not in alpha and "error" not in alpha, alpha
            finally:
                release.release()
            client.receive(alpha)
            assert alpha["result"] == {
                "content": [{"type": "text", "text": "alpha returned\n"}],
                "isError": False,
            }, alpha
            return client.finish()


@executions(RUNTIME)
def test_restarting_named_session_defers_replacement_until_code(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with McpClient(
            binary,
            execution.serve(),
            selected_python(root, Path(sys.executable)),
            root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(python="value = 'default state'")
            client.expect(python="value = 'old state'", session="alpha")
            result = client.send(control="restart", session="alpha")
            assert result.get("isError") is not True, result
            assert "[worker stopped: in-memory state lost]" in last_result_text(client)
            assert "[starting new worker]" not in last_result_text(client)
            active = client.send(timeout_ms=0)
            assert active == {
                "content": [{"type": "text", "text": "\n[idle]"}],
                "isError": False,
            }, active
            client.send(session="alpha", timeout_ms=0)
            assert last_result_text(client) == "\n[stopped]"
            client.expect("'default state'\n", python="value")
            client.expect(
                "[starting new worker]\n'new state'\n",
                # fmt: python
                python=code("""
                    assert "value" not in globals()
                    value = "new state"
                    value
                    """),
                session="alpha",
            )
            return client.finish()


@requires(POSIX, NATIVE_FIXTURES, PROCESS_EVENTS)
@executions(RUNTIME)
def test_cancelled_aggregate_poll_omits_empty_stopped_session(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary).resolve()
        armed = root / "write-armed"
        with (
            closing(FifoCheckpoint.create(root / "default-ready")) as ready,
            closing(FifoCheckpoint.create(root / "default-release")) as release,
            closing(FifoCheckpoint.create(root / "write-reached")) as write_reached,
            closing(FifoCheckpoint.create(root / "write-release")) as write_release,
            McpClient(
                binary,
                execution.serve(
                    *(("--writable-root", str(root)) if execution == SANDBOXED else ())
                ),
                selected_python(root, Path(sys.executable))
                | {
                    LOADER_VARIABLE: str(
                        build_interposer(root, "response_write_interposer")
                    ),
                    "MCP_CONSOLE_TEST_RESPONSE_WRITE_REACHED": str(write_reached.path),
                    "MCP_CONSOLE_TEST_RESPONSE_WRITE_RELEASE": str(write_release.path),
                    "MCP_CONSOLE_TEST_RESPONSE_WRITE_MATCH": "[running; poll with an empty send]",
                    "MCP_CONSOLE_TEST_RESPONSE_WRITE_COMPLETE": "1",
                    "MCP_CONSOLE_TEST_RESPONSE_WRITE_ARMED": str(armed),
                },
                root,
            ) as client,
        ):
            client.transcript.append(
                {
                    "scenario": "Cancelling an aggregate poll while one session is running must not restore an empty stopped session's notice.",
                    "fixture": "The default Python cell waits at a FIFO. Its first complete response write is held until a ping is received, so the ping proves delivery settled before the aggregate starts. A targeted poll proves aggregate ownership before cancellation; alpha has stopped with all output delivered.",
                }
            )
            client.initialize_and_list_tools()
            client.expect(python="pass")
            client.expect(python="pass", session="alpha")
            client.expect(
                "[worker stopped: in-memory state lost]\n[stopped]",
                control="restart",
                session="alpha",
            )
            try:
                armed.touch()
                # The held write returns only on release, so the recording
                # cannot yet hold this reply's delivery.
                client.recording_receipts = False
                started = client.send(
                    timeout_ms=0,
                    # fmt: python
                    python=code("""
                        with open("default-ready", "wb", buffering=0) as ready:
                            _ = ready.write(b"1")
                        with open("default-release", "rb", buffering=0) as release:
                            assert release.read(1) == b"1"
                        print("default returned")
                        """),
                )
                client.recording_receipts = True
                running = {
                    "content": [{"type": "text", "text": RUNNING}],
                    "isError": False,
                }
                assert without_elapsed_result(started) == running, started
                started_id = client.transcript[-1]["id"]
                ready.wait("default is blocked before completion", client=client)
                write_reached.wait("initial running response has an unsettled write")
                barrier = client.start_request("ping")
                await_event(
                    client,
                    root,
                    "protocol",
                    lambda event: (
                        event["direction"] == "received"
                        and event["message"].get("id") == barrier["id"]
                    ),
                )
                write_release.release()
                client.receive(barrier)
                assert barrier["result"] == {}, barrier
                delivered = await_event(
                    client,
                    root,
                    "delivery",
                    lambda event: event["request_id"] == started_id,
                )
                assert delivered["delivered"] is True, delivered
                pending = client.start_send(timeout_ms=60_000)
                deadline = client.response_deadline()
                first_probe = len(client.transcript)
                while True:
                    observed = client.send(session="default", timeout_ms=0)
                    if observed.get("isError"):
                        assert observed == {
                            "content": [
                                {
                                    "type": "text",
                                    "text": "[worker evaluation is already being polled]",
                                }
                            ],
                            "isError": True,
                        }, observed
                        break
                    assert without_elapsed_result(observed) == running, observed
                    assert time.monotonic() < deadline, (
                        "aggregate did not claim default"
                    )
                # Keep the ownership receipt; earlier successful probes only
                # reflect scheduling before the aggregate claimed the wait.
                client.transcript[first_probe:] = [client.transcript[-1]]
                client.notify("notifications/cancelled", requestId=pending["id"])
                assert client.request("ping")["result"] == {}
                call = await_event(
                    client,
                    root,
                    "tool_call",
                    lambda event: event["request_id"] == pending["id"],
                )
                cancelled = await_event(
                    client,
                    root,
                    "tool_result",
                    lambda event: event["call_id"] == call["call_id"],
                )
                assert cancelled["error"] == {
                    "code": -32603,
                    "message": "request cancelled; poll with an empty send for any admitted cell",
                }, cancelled
                assert "result" not in pending, pending
            finally:
                write_release.release()
                release.release()
            result = client.send(timeout_ms=60_000)
            assert result == {
                "content": [{"type": "text", "text": "default returned\n"}],
                "isError": False,
            }, result
            assert client.send(timeout_ms=0) == {
                "content": [{"type": "text", "text": "\n[idle]"}],
                "isError": False,
            }
            return client.finish()


@executions(RUNTIME)
def test_interrupt_only_targets_selected_session(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with McpClient(
            binary,
            execution.serve(),
            selected_python(root, Path(sys.executable)),
            root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(python="default_value = 42")
            for session in ("alpha", "beta"):
                client.expect(
                    f'[input requested: "{session}> "]\n[waiting for stdin]',
                    # fmt: python
                    python=code(f"""
                        retained = "{session}"
                        answer = input("{session}> ")
                        """),
                    session=session,
                )
            interrupted = client.send(control="interrupt", session="alpha")
            assert interrupted.get("isError") is not True, interrupted
            assert "KeyboardInterrupt" in last_result_text(client), interrupted
            client.expect(
                "('alpha', False)\n",
                python="retained, 'answer' in globals()",
                session="alpha",
            )
            client.send(session="beta", timeout_ms=0)
            assert last_result_text(client) == "\n[waiting for stdin]"
            client.expect(stdin="beta survived\n", session="beta")
            client.expect(
                "('beta', 'beta survived')\n", python="retained, answer", session="beta"
            )
            return client.finish()


@executions(RUNTIME)
def test_failed_named_session_waits_for_new_code(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with McpClient(
            binary,
            execution.serve(),
            selected_python(root, Path(sys.executable)),
            root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(python="value = 'default state'")
            client.expect(python="value = 'old state'", session="alpha")
            failed = client.send(python="import os; os._exit(86)", session="alpha")
            assert failed["isError"] is True, failed
            assert "[worker stopped: in-memory state lost]" in last_result_text(client)
            assert "[starting new worker]" not in last_result_text(client)
            active = client.send(timeout_ms=0)
            assert active == {
                "content": [{"type": "text", "text": "\n[idle]"}],
                "isError": False,
            }, active
            client.expect("'default state'\n", python="value")
            client.expect(
                "[starting new worker]\nFalse\n",
                python="'value' in globals()",
                session="alpha",
            )
            return client.finish()


@requires(POSIX)
@executions(RUNTIME)
def test_aggregate_poll_omits_delivered_named_discovery_failure(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        python = root / "selected-python"
        python.symlink_to(sys.executable)
        environment = without_r(root)
        environment["RETICULATE_PYTHON"] = str(python)
        with McpClient(binary, execution.serve(), environment, root) as client:
            client.initialize_and_list_tools()
            client.expect(python="default_value = 42")
            python.unlink()
            failure = client.send(session="alpha", python="rejected_cell = True")
            assert failure.get("isError"), failure
            assert "selected Python executable is not an absolute file" in str(failure)
            active = {
                "content": [{"type": "text", "text": "\n[idle]"}],
                "isError": False,
            }
            assert client.send(timeout_ms=0) == active
            assert client.send(timeout_ms=0) == active
            assert client.send(session="alpha", timeout_ms=0) == failure
            client.expect("42\n", python="default_value")
            transcript, stderr = client.finish_with_standard_error(
                expected_exit_status=1
            )
            assert (
                stderr
                == f"selected Python executable is not an absolute file: {python}\n"
            )
            return json.loads(
                json.dumps(transcript + [{"stderr": stderr}]).replace(
                    str(root), "<workspace>"
                )
            )


if __name__ == "__main__":
    run_this_suite(__file__)
