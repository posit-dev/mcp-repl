"""Integrator control shares runtime state without consuming MCP responses."""

import json
import os
import socket
import stat
import subprocess
import sys
import tempfile
import time
from contextlib import closing
from pathlib import Path
from typing import Any, Self

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.output.test_recovery import cancel_result
from boundaries.client_server.python.test_without_r import selected_environment
from support.assertions import release_worker_callback_gate
from support.allocations import AllocationProfile
from support.checkpoints import FifoCheckpoint, wait_for_checkpoint
from support.client import McpClient, TextReader
from support.execution import DIRECT, SANDBOXED, Execution, executions
from support.normalization import code, normalize_recording
from support.previews import recording_events
from support.records import Transcript, TranscriptWithCompanions
from support.requirements import NATIVE_FIXTURES, POSIX, R, R_EVENT_LOOP, requires
from support.suites import run_this_suite


class ControlClient:
    def __init__(self, path: Path) -> None:
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(30)
        self.socket.connect(str(path))
        self.reader = TextReader(self.socket)
        self.transcript: list[dict[str, Any]] = []

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *args: object) -> None:
        self.reader.close()

    def start(self, **request: Any) -> dict[str, Any]:
        request = {"id": str(len(self.transcript) + 1), **request}
        self.socket.sendall(json.dumps(request).encode() + b"\n")
        entry = {"request": request}
        self.transcript.append(entry)
        return entry

    def receive(
        self,
        entry: dict[str, Any],
        *,
        invalid: bool = False,
        timeout: float = 60,
    ) -> dict[str, Any]:
        response = json.loads(self.reader.readline(timeout=timeout))
        assert response["id"] == (None if invalid else entry["request"]["id"]), response
        assert response.keys() in ({"id", "result"}, {"id", "error"}), response
        entry["response"] = response
        return response

    def request(
        self, *, response_timeout: float = 60, **request: Any
    ) -> dict[str, Any]:
        return self.receive(self.start(**request), timeout=response_timeout)

    def wait_for_idle(self, client: McpClient, *, session: str = "default") -> None:
        """Observe delivery settlement before a nonwaiting integrator operation."""
        deadline = client.response_deadline()
        first = len(self.transcript)
        while True:
            remaining = deadline - time.monotonic()
            assert remaining > 0, "MCP response delivery did not settle"
            response = self.request(
                op="status", session=session, response_timeout=remaining
            )
            status = response["result"]
            if status["state"] == "idle":
                assert status == {"state": "idle", "idle": True, "evaluation": None}, (
                    status
                )
                break
            assert status["state"] in {"busy", "awaiting_result"}, status
        # Every intermediate observation was checked. Retain the exact final
        # status while keeping scheduling-dependent poll counts out of snapshots.
        observed = self.transcript[first]
        observed["response"] = {**response, "id": observed["request"]["id"]}
        self.transcript[first:] = [observed]


def result_text(response: dict[str, Any]) -> str:
    result = response["result"]
    assert not result.get("isError"), result
    assert all(block["type"] == "text" for block in result["content"]), result
    return "".join(block["text"] for block in result["content"])


def wait_for_pending_text(endpoint: Path, text: str) -> None:
    """Wait until the server holds `text` as pending output, without collecting it."""
    with ControlClient(endpoint) as observer:
        subscribed = observer.request(op="subscribe")
        assert subscribed.get("result") == {"subscribed": True}, subscribed
        while True:
            # Each frame replaces the previous preview; the latest retains text.
            event = json.loads(observer.reader.readline(timeout=30))
            assert event["event"] == "preview", event
            if text in result_text(event):
                return


def wait_for_queued_integrator(client: McpClient, root: Path, source: str) -> None:
    recordings = root / ".agents/console/sessions"

    def queued() -> Path | None:
        for path in recordings.glob("*/capture/events.jsonl"):
            # Filesystem observations can see the final line before its writer
            # has appended the delimiter. Only complete journal records count.
            events = [
                json.loads(line)
                for line in path.read_bytes().splitlines(keepends=True)
                if line.endswith(b"\n")
            ]
            calls = {
                event["call_id"]
                for event in events
                if event["event"] == "tool_call"
                and source in event["request"]["arguments"].values()
            }
            if any(
                event.get("direction") == "integrator_waiting"
                and event["message"]["call_id"] in calls
                for event in events
            ):
                return path
        return None

    wait_for_checkpoint(
        queued,
        "integrator request waiting in its captured generation",
        root=recordings,
        recursive=True,
        watch_files=lambda: recordings.glob("*/capture/events.jsonl"),
        client=client,
    )


def wait_for_cell_started(client: McpClient, root: Path, source: str) -> None:
    """Observe controller entry separately from a worker-to-host checkpoint."""
    recordings = root / ".agents/console/sessions"

    def started() -> Path | None:
        for path in recordings.glob("*/capture/events.jsonl"):
            # A live journal can end inside a record or a UTF-8 character.
            events = [
                json.loads(line)
                for line in path.read_bytes().splitlines(keepends=True)
                if line.endswith(b"\n")
            ]
            calls = {
                event["call_id"]
                for event in events
                if event["event"] == "tool_call"
                and source in event["request"]["arguments"].values()
            }
            if any(
                event["event"] == "cell_started" and event["call_id"] in calls
                for event in events
            ):
                return path
        return None

    wait_for_checkpoint(
        started,
        "controller observed foreground cell entry",
        root=recordings,
        recursive=True,
        watch_files=lambda: recordings.glob("*/capture/events.jsonl"),
        client=client,
    )


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_evaluates_shared_state_without_mcp_output(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with McpClient(
            binary,
            execution.serve("--control-socket", str(endpoint)),
            selected_environment(root),
            root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect("agent ready\n", python="shared = 40; print('agent ready')")
            with ControlClient(endpoint) as control:
                assert stat.S_IMODE(endpoint.stat().st_mode) == 0o600
                result = control.request(
                    op="evaluate",
                    language="python",
                    code="shared += 2; print('integrator output')",
                    busy="wait",
                    wait_ms=30000,
                )
                assert result_text(result) == "integrator output\n", result
                status = control.request(op="status")
                assert status["result"]["state"] == "idle", status
                client.expect("\n[idle]")
                client.expect("42\n", python="print(shared)")
                transcript = client.finish()
            assert not endpoint.exists(), "server left its control socket behind"
            return TranscriptWithCompanions(
                transcript, {"control.yaml": control.transcript}
            )


@requires(POSIX)
@executions(DIRECT)
def test_rejects_invalid_requests_before_execution(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with McpClient(
            binary,
            execution.serve("--control-socket", str(endpoint)),
            selected_environment(root),
            root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(python="sentinel = 0")
            with ControlClient(endpoint) as control:
                malformed = [
                    {"op": "status", "extra": True},
                    {"op": "unknown"},
                    {"op": "evaluate", "language": "julia", "code": "sentinel = 1"},
                    {
                        "op": "evaluate",
                        "language": "python",
                        "code": "sentinel = 1",
                        "busy": "skip",
                    },
                    {
                        "op": "evaluate",
                        "language": "python",
                        "code": "sentinel = 1",
                        "record": "false",
                    },
                ]
                for request in malformed:
                    response = control.receive(control.start(**request), invalid=True)
                    assert response["error"]["code"] == "invalid_request", response
                for frame in (
                    "\n",
                    '{"id":"duplicate","op":"evaluate","language":"python","code":"sentinel = 1","code":"sentinel = 2"}\n',
                    '{"id":"duplicate","op":"status","op":"preview"}\n',
                ):
                    entry = {"request": {"wire": frame}}
                    control.transcript.append(entry)
                    control.socket.sendall(frame.encode())
                    response = control.receive(entry, invalid=True)
                    assert response["error"]["code"] == "invalid_request", response
                for policy in ({"wait_ms": 10}, {"busy": "wait"}):
                    response = control.request(
                        op="evaluate", language="python", code="sentinel = 1", **policy
                    )
                    assert response["error"]["code"] == "invalid_request", response
                # A split Unicode code point and multiple frames must preserve
                # framing without requiring one write per request.
                request = {
                    "id": "split",
                    "op": "evaluate",
                    "language": "python",
                    "code": "print('🦀')",
                    "busy": "wait",
                    "wait_ms": 30000,
                }
                frame = json.dumps(request, ensure_ascii=False).encode() + b"\n"
                split = frame.index("🦀".encode()) + 1
                control.socket.sendall(frame[:split])
                control.socket.sendall(
                    frame[split:] + b'{"id":"after","op":"status"}\n'
                )
                entry = {"request": request}
                control.transcript.append(entry)
                assert result_text(control.receive(entry)) == "🦀\n"
                entry = {"request": {"id": "after", "op": "status"}}
                control.transcript.append(entry)
                assert control.receive(entry)["result"]["state"] == "idle"
                client.expect("0\n", python="print(sentinel)")
                transcript = client.finish()
            return TranscriptWithCompanions(
                transcript, {"control.yaml": control.transcript}
            )


@requires(POSIX, R)
@executions(DIRECT)
def test_evaluates_r_on_the_session_thread(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        endpoint = root / "control.sock"
        with McpClient(
            binary,
            execution.serve("--control-socket", str(endpoint)),
            current_directory=root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(r="shared <- 40L")
            with ControlClient(endpoint) as control:
                result = control.request(
                    op="evaluate",
                    language="r",
                    code='shared <- shared + 2L; cat("integrator R\\n")',
                    busy="wait",
                    wait_ms=30000,
                )
                assert result_text(result) == "integrator R\n", result
                client.expect("[1] 42\n", r="shared")
                transcript = client.finish()
            return TranscriptWithCompanions(
                transcript, {"control.yaml": control.transcript}
            )


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_previews_and_busy_policies_preserve_mcp_delivery(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        arguments = ["--control-socket", str(endpoint)]
        if execution == SANDBOXED:
            arguments += ["--writable-root", str(root)]
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            McpClient(
                binary, execution.serve(*arguments), selected_environment(root), root
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.expect(python="sentinel = 0")
            # fmt: python
            source = code(f"""
                print("agent pending", flush=True)
                with open({str(entered.path)!r}, "wb", buffering=0) as gate:
                    gate.write(b"1")
                with open({str(release.path)!r}, "rb", buffering=0) as gate:
                    gate.read(1)
                print("agent finished")
                """)
            pending = client.start_send(python=source, timeout_ms=60000)
            try:
                entered.wait(client=client)
                recordings = root / ".agents/console/sessions"
                wait_for_checkpoint(
                    lambda: next(
                        (
                            path
                            for path in recordings.glob(
                                "*/capture/sessions/*/cell-*.txt"
                            )
                            if path.read_text() == "agent pending\n"
                        ),
                        None,
                    ),
                    "agent output captured before integrator preview",
                    root=recordings,
                    recursive=True,
                    watch_files=lambda: recordings.glob(
                        "*/capture/sessions/*/cell-*.txt"
                    ),
                    client=client,
                )
                with ControlClient(endpoint) as control:
                    status = control.request(op="status")
                    assert status["result"]["state"] == "evaluating", status
                    assert not status["result"]["idle"], status
                    assert status["result"]["evaluation"]["origin"] == "mcp", status
                    previews = [control.request(op="preview") for _ in range(2)]
                    assert result_text(previews[0]) == "agent pending\n", previews
                    assert previews[0]["result"] == previews[1]["result"], previews
                    rejected = control.request(
                        op="evaluate", language="python", code="sentinel = 1"
                    )
                    assert rejected["error"]["code"] == "busy", rejected
                    timed_out = control.request(
                        op="evaluate",
                        language="python",
                        code="sentinel = 2",
                        busy="wait",
                        wait_ms=0,
                    )
                    assert timed_out["error"]["code"] == "timeout", timed_out
                    queued_source = "sentinel += 10; print('queued integrator')"
                    queued = control.start(
                        op="evaluate",
                        language="python",
                        code=queued_source,
                        busy="wait",
                        wait_ms=30000,
                    )
                    wait_for_queued_integrator(client, root, queued_source)
                    release.release()
                    client.receive(pending)
                    assert result_text(pending) == "agent pending\nagent finished\n", (
                        pending
                    )
                    assert result_text(control.receive(queued)) == "queued integrator\n"
                    client.expect("10\n", python="print(sentinel)")
                    transcript = normalize_recording(client.finish(), root)
                return TranscriptWithCompanions(
                    transcript, {"control.yaml": control.transcript}
                )
            finally:
                release.release()


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_projection_omission_keeps_journal_and_raw_output(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with McpClient(
            binary,
            execution.serve("--control-socket", str(endpoint)),
            selected_environment(root),
            root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(python="pass")
            with ControlClient(endpoint) as control:
                hidden_source = "hidden_value = 42; print('hidden integrator output')"
                visible_source = 'print("visible integrator output")'
                assert (
                    result_text(
                        control.request(
                            op="evaluate",
                            language="python",
                            code=hidden_source,
                            record=False,
                            busy="wait",
                            wait_ms=30000,
                        )
                    )
                    == "hidden integrator output\n"
                )
                assert (
                    result_text(
                        control.request(
                            op="evaluate", language="python", code=visible_source
                        )
                    )
                    == "visible integrator output\n"
                )
                client.expect("42\n", python="print(hidden_value)")
                transcript = normalize_recording(client.finish(), root)
            (recording,) = (root / ".agents/console/sessions").iterdir()
            journal = (recording / "capture/events.jsonl").read_text()
            markdown = (recording / "transcript.md").read_text()
            (source_document,) = (recording / "sessions").glob("*.qmd")
            document = source_document.read_text()
            calls = [
                event
                for event in recording_events(recording)
                if event["event"] == "tool_call"
            ]
            sources = [event["request"]["arguments"].get("python") for event in calls]
            assert hidden_source in sources and visible_source in sources, calls
            assert "hidden integrator output" in journal, journal
            assert "hidden integrator output" not in markdown, markdown
            assert hidden_source not in document, document
            assert (
                visible_source in document and "visible integrator output" in markdown
            ), (document, markdown)
            assert "Integrator call:" in markdown, markdown
            raw = "".join(
                path.read_text()
                for path in (recording / "capture/sessions").glob("*/*.txt")
            )
            assert "hidden integrator output\n" in raw, raw
            (hidden_call,) = [
                event
                for event in calls
                if event["request"]["arguments"].get("python") == hidden_source
            ]
            assert hidden_call["origin"] == "integrator", hidden_call
            assert hidden_call["omit_from_projections"] is True, hidden_call
            (hidden_result,) = [
                event
                for event in recording_events(recording)
                if event["event"] == "tool_result"
                and event["call_id"] == hidden_call["call_id"]
            ]
            assert hidden_result["omit_from_projections"] is True, hidden_result
            document = document.replace(str(root.resolve()), "<workspace>").replace(
                str(root), "<workspace>"
            )
            return TranscriptWithCompanions(
                transcript, {"control.yaml": control.transcript, "qmd": document}
            )


@requires(POSIX)
@executions(DIRECT)
def test_inspects_requirements_and_manifest(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with McpClient(
            binary,
            execution.serve("--control-socket", str(endpoint)),
            selected_environment(root),
            root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(python="pass")
            expected = client.send(requirements={"action": "get"})
            assert not expected.get("isError"), expected
            with ControlClient(endpoint) as control:
                requirements = control.request(op="requirements")
                assert requirements["result"] == expected["structuredContent"], (
                    requirements,
                    expected,
                )
                manifest = control.request(op="manifest")
                assert (
                    manifest["result"] == expected["structuredContent"]["requirements"]
                ), (manifest, expected)
                transcript = normalize_recording(client.finish(), root)
            return TranscriptWithCompanions(
                transcript,
                {"control.yaml": normalize_recording(control.transcript, root)},
            )


@requires(POSIX)
@executions(DIRECT)
def test_mcp_poll_cannot_claim_integrator_output(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            McpClient(
                binary,
                execution.serve("--control-socket", str(endpoint)),
                selected_environment(root),
                root,
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.expect(python="pass")
            # fmt: python
            source = code(f"""
                print("integrator pending", flush=True)
                with open({str(entered.path)!r}, "wb", buffering=0) as gate:
                    gate.write(b"1")
                with open({str(release.path)!r}, "rb", buffering=0) as gate:
                    gate.read(1)
                print("integrator finished")
                """)
            with ControlClient(endpoint) as control:
                pending = control.start(
                    op="evaluate",
                    language="python",
                    code=source,
                    busy="wait",
                    wait_ms=30000,
                )
                try:
                    entered.wait(client=client)
                    response = client.send(session="default")
                    assert response.get("isError"), response
                    assert "integrator pending" not in json.dumps(response), response
                    release.release()
                    assert (
                        result_text(control.receive(pending))
                        == "integrator pending\nintegrator finished\n"
                    )
                    client.expect("\n[idle]")
                    transcript = normalize_recording(client.finish(), root)
                finally:
                    release.release()
            return TranscriptWithCompanions(
                transcript,
                {"control.yaml": normalize_recording(control.transcript, root)},
            )


@requires(POSIX)
@executions(DIRECT)
@requires(NATIVE_FIXTURES)
def test_cancelled_mcp_control_keeps_recovery_out_of_integrator_result(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            closing(FifoCheckpoint.create(root / "result-reached")) as result_reached,
            closing(FifoCheckpoint.create(root / "result-release")) as result_release,
            closing(AllocationProfile(root)) as profile,
            McpClient(
                binary,
                execution.serve("--control-socket", str(endpoint)),
                {
                    **selected_environment(root),
                    **profile.environment,
                    "MCP_CONSOLE_TEST_RESULT_REACHED": str(result_reached.path),
                    "MCP_CONSOLE_TEST_RESULT_RELEASE": str(result_release.path),
                },
                root,
            ) as client,
        ):
            client.transcript.append(
                {
                    "scenario": "Cancelled MCP control keeps its error separate from an integrator cell.",
                    "fixture": "The Python cell ignores SIGINT and waits at a FIFO. A native checkpoint holds the write of the MCP result so cancellation follows result assembly.",
                }
            )
            client.initialize_and_list_tools()
            client.expect(python="sentinel = 0")
            # fmt: python
            source = code(f"""
                import signal

                previous_handler = signal.signal(signal.SIGINT, signal.SIG_IGN)
                print("integrator pending", flush=True)
                with open({str(entered.path)!r}, "wb", buffering=0) as gate:
                    gate.write(b"1")
                with open({str(release.path)!r}, "rb", buffering=0) as gate:
                    gate.read(1)
                signal.signal(signal.SIGINT, previous_handler)
                print("integrator finished")
                """)
            with (
                ControlClient(endpoint) as control,
                ControlClient(endpoint) as inspector,
            ):
                # Receiving the MCP setup response can precede its delivery
                # acknowledgment. Queue setup behind that retained owner.
                pending = control.start(
                    op="evaluate",
                    language="python",
                    code=source,
                    busy="wait",
                    wait_ms=30000,
                )
                try:
                    entered.wait(client=client)
                    interrupted = []
                    recordings = root / ".agents/console/sessions"
                    wait_for_checkpoint(
                        lambda: next(
                            (
                                path
                                for path in recordings.glob(
                                    "*/capture/sessions/*/cell-*.txt"
                                )
                                if path.read_text() == "integrator pending\n"
                            ),
                            None,
                        ),
                        "integrator output captured before preview",
                        root=recordings,
                        recursive=True,
                        watch_files=lambda: recordings.glob(
                            "*/capture/sessions/*/cell-*.txt"
                        ),
                        client=client,
                    )
                    profile.pause_results(True)
                    for _ in range(2):
                        request = client.start_send(
                            control="interrupt", python="sentinel = 99"
                        )
                        interrupted.append(request)
                        cancel_result(client, request, result_reached, result_release)
                    profile.pause_results(False)
                    preview = inspector.request(op="preview")
                    release.release()
                    result = control.receive(pending)
                    assert (
                        result_text(result)
                        == "integrator pending\nintegrator finished\n"
                    ), result
                    assert result_text(preview) == "integrator pending\n", preview
                    recovered = client.send(session="default")
                    assert recovered == {
                        "content": [
                            {
                                "type": "text",
                                "text": "\n".join(
                                    [
                                        "interrupted evaluation is still active; cell was not run"
                                    ]
                                    * 2
                                ),
                            }
                        ],
                        "isError": True,
                    }, recovered
                    client.expect("\n[idle]")
                    client.expect("0\n", python="print(sentinel)")
                    transcript = normalize_recording(client.finish(), root)
                finally:
                    profile.pause_results(False)
                    result_release.release()
                    release.release()
            return TranscriptWithCompanions(
                transcript,
                {
                    "control.yaml": normalize_recording(control.transcript, root),
                    "preview.yaml": inspector.transcript,
                },
            )


@requires(POSIX)
@executions(DIRECT)
def test_disconnect_withdraws_waiting_code(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            McpClient(
                binary,
                execution.serve("--control-socket", str(endpoint)),
                selected_environment(root),
                root,
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.expect(python="sentinel = 0")
            # fmt: python
            source = code(f"""
                with open({str(entered.path)!r}, "wb", buffering=0) as gate:
                    gate.write(b"1")
                with open({str(release.path)!r}, "rb", buffering=0) as gate:
                    gate.read(1)
                print("agent finished")
                """)
            pending = client.start_send(python=source, timeout_ms=60000)
            try:
                entered.wait(client=client)
                with ControlClient(endpoint) as control:
                    queued_source = "sentinel = 99"
                    control.start(
                        op="evaluate",
                        language="python",
                        code=queued_source,
                        busy="wait",
                        wait_ms=30000,
                    )
                    wait_for_queued_integrator(client, root, queued_source)
                recordings = root / ".agents/console/sessions"
                wait_for_checkpoint(
                    lambda: next(
                        (
                            path
                            for path in recordings.glob("*/capture/events.jsonl")
                            if '"control connection closed; code was not run"'
                            in path.read_text()
                        ),
                        None,
                    ),
                    "disconnected request settled before releasing the runner",
                    root=recordings,
                    recursive=True,
                    watch_files=lambda: recordings.glob("*/capture/events.jsonl"),
                    client=client,
                )
                release.release()
                client.receive(pending)
                assert result_text(pending) == "agent finished\n", pending
                client.expect("0\n", python="print(sentinel)")
                transcript = normalize_recording(client.finish(), root)
            finally:
                release.release()
            return TranscriptWithCompanions(
                transcript, {"control.yaml": control.transcript}
            )


@requires(POSIX)
@executions(DIRECT)
def test_disconnect_keeps_admitted_result_journaled(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            McpClient(
                binary,
                execution.serve("--control-socket", str(endpoint)),
                selected_environment(root),
                root,
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.expect(python="pass")
            # fmt: python
            source = code(f"""
                print("private before", flush=True)
                with open({str(entered.path)!r}, "wb", buffering=0) as gate:
                    gate.write(b"1")
                with open({str(release.path)!r}, "rb", buffering=0) as gate:
                    gate.read(1)
                print("private after")
                """)
            try:
                with ControlClient(endpoint) as control:
                    control.start(
                        op="evaluate",
                        language="python",
                        code=source,
                        record=False,
                        busy="wait",
                        wait_ms=30000,
                    )
                    entered.wait(client=client)
                release.release()
                recordings = root / ".agents/console/sessions"

                def completed() -> Path | None:
                    for path in recordings.glob("*/capture/events.jsonl"):
                        for line in path.read_bytes().splitlines(keepends=True):
                            if not line.endswith(b"\n"):
                                continue
                            event = json.loads(line)
                            if event["event"] == "tool_result" and event.get(
                                "omit_from_projections"
                            ):
                                content = event.get("result", {}).get("content", [])
                                if (
                                    "".join(block.get("text", "") for block in content)
                                    == "private before\nprivate after\n"
                                ):
                                    return path
                    return None

                wait_for_checkpoint(
                    completed,
                    "admitted result recorded after socket disconnect",
                    root=recordings,
                    recursive=True,
                    watch_files=lambda: recordings.glob("*/capture/events.jsonl"),
                    client=client,
                )
                client.expect("\n[idle]")
                client.expect("survived\n", python="print('survived')")
                transcript = normalize_recording(client.finish(), root)
                (recording,) = recordings.iterdir()
                markdown = (recording / "transcript.md").read_text()
                assert (
                    "private before" not in markdown and "private after" not in markdown
                ), markdown
            finally:
                release.release()
            return TranscriptWithCompanions(
                transcript,
                {"control.yaml": normalize_recording(control.transcript, root)},
            )


@requires(POSIX)
@executions(DIRECT)
def test_restart_rejects_waiting_code_from_old_generation(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            McpClient(
                binary,
                execution.serve("--control-socket", str(endpoint)),
                selected_environment(root),
                root,
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.expect(python="pass")
            # fmt: python
            source = code(f"""
                with open({str(entered.path)!r}, "wb", buffering=0) as gate:
                    gate.write(b"1")
                with open({str(release.path)!r}, "rb", buffering=0) as gate:
                    gate.read(1)
                """)
            pending = client.start_send(python=source, timeout_ms=60000)
            try:
                entered.wait(client=client)
                with ControlClient(endpoint) as control:
                    queued_source = "must_not_run = True"
                    queued = control.start(
                        op="evaluate",
                        language="python",
                        code=queued_source,
                        busy="wait",
                        wait_ms=30000,
                    )
                    wait_for_queued_integrator(client, root, queued_source)
                    restart = client.start_send(
                        control="restart", python="print('replacement ready')"
                    )
                    client.receive_many([pending, restart])
                    assert pending["result"].get("isError"), pending
                    assert not restart["result"].get("isError"), restart
                    rejected = control.receive(queued)
                    assert rejected["error"]["code"] == "execution_failed", rejected
                    client.expect(
                        "False\n", python="print('must_not_run' in globals())"
                    )
                    transcript = normalize_recording(client.finish(), root)
            finally:
                release.release()
            return TranscriptWithCompanions(
                transcript, {"control.yaml": control.transcript}
            )


@requires(POSIX)
@executions(DIRECT)
def test_worker_crash_rejects_waiting_code_from_old_connection(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            McpClient(
                binary,
                execution.serve("--control-socket", str(endpoint)),
                selected_environment(root),
                root,
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.expect(python="pass")
            # fmt: python
            source = code(f"""
                import os
                with open({str(entered.path)!r}, "wb", buffering=0) as gate:
                    gate.write(b"1")
                with open({str(release.path)!r}, "rb", buffering=0) as gate:
                    gate.read(1)
                os._exit(17)
                """)
            pending = client.start_send(python=source, timeout_ms=60000)
            try:
                entered.wait(client=client)
                with ControlClient(endpoint) as control:
                    queued_source = "must_not_run = True"
                    queued = control.start(
                        op="evaluate",
                        language="python",
                        code=queued_source,
                        busy="wait",
                        wait_ms=30000,
                    )
                    wait_for_queued_integrator(client, root, queued_source)
                    release.release()
                    client.receive(pending)
                    assert pending["result"].get("isError"), pending
                    rejected = control.receive(queued)
                    assert "error" in rejected, rejected
                    assert rejected["error"]["code"] == "execution_failed", rejected
                    client.expect(
                        "False\n", python="print('must_not_run' in globals())"
                    )
                    transcript = normalize_recording(client.finish(), root)
            finally:
                release.release()
            return TranscriptWithCompanions(
                transcript, {"control.yaml": control.transcript}
            )


@requires(POSIX)
@executions(DIRECT)
def test_restart_keeps_integrator_output_out_of_mcp(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            McpClient(
                binary,
                execution.serve("--control-socket", str(endpoint)),
                selected_environment(root),
                root,
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.expect(python="pass")
            # fmt: python
            source = code(f"""
                print("integrator pending", flush=True)
                with open({str(entered.path)!r}, "wb", buffering=0) as gate:
                    gate.write(b"1")
                with open({str(release.path)!r}, "rb", buffering=0) as gate:
                    gate.read(1)
                """)
            with ControlClient(endpoint) as control:
                pending = control.start(
                    op="evaluate",
                    language="python",
                    code=source,
                    busy="wait",
                    wait_ms=30000,
                )
                try:
                    entered.wait(client=client)
                    restarted = client.send(
                        control="restart", python="print('replacement ready')"
                    )
                    assert not restarted.get("isError"), restarted
                    assert "integrator pending" not in json.dumps(restarted), restarted
                    stopped = control.receive(pending)
                    assert stopped["result"].get("isError"), stopped
                    text = "".join(
                        block["text"] for block in stopped["result"]["content"]
                    )
                    assert text.startswith("integrator pending\n"), stopped
                    assert (
                        "stopped by session restart request before evaluation finished"
                        in text
                    ), stopped
                    client.expect("\n[idle]")
                    client.expect("survived\n", python="print('survived')")
                    transcript = normalize_recording(client.finish(), root)
                finally:
                    release.release()
            return TranscriptWithCompanions(
                transcript,
                {"control.yaml": normalize_recording(control.transcript, root)},
            )


@requires(POSIX)
def test_preserves_existing_socket_path(binary: Path) -> Transcript:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        endpoint = root / "control.sock"
        endpoint.write_text("belongs to the integrator\n")
        result = subprocess.run(
            [binary, "serve", "--no-sandbox", "--control-socket", str(endpoint)],
            input="",
            text=True,
            capture_output=True,
            timeout=30,
            check=False,
            env=dict(os.environ, MCP_CONSOLE_HOME=str(root)),
            cwd=root,
        )
        assert result.returncode != 0, result
        assert endpoint.read_text() == "belongs to the integrator\n"
        assert not result.stdout, result
        return [
            {
                "exit_code": result.returncode,
                "stderr": result.stderr.replace(str(root), "<workspace>"),
            }
        ]


@requires(POSIX)
def test_requires_private_socket_directory(binary: Path) -> Transcript:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        root.chmod(0o755)
        endpoint = root / "control.sock"
        result = subprocess.run(
            [binary, "serve", "--no-sandbox", "--control-socket", str(endpoint)],
            input="",
            text=True,
            capture_output=True,
            timeout=30,
            check=False,
            env=dict(os.environ, MCP_CONSOLE_HOME=str(root)),
            cwd=root,
        )
        assert result.returncode != 0, result
        assert not endpoint.exists()
        assert not result.stdout, result
        return [
            {
                "exit_code": result.returncode,
                "stderr": result.stderr.replace(str(root), "<workspace>"),
            }
        ]


@requires(NATIVE_FIXTURES, R_EVENT_LOOP)
def test_preview_keeps_recovered_output_before_later_idle_output(
    binary: Path,
) -> Transcript:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        endpoint = root / "control.sock"
        with (
            closing(FifoCheckpoint.create(root / "result-reached")) as result_reached,
            closing(FifoCheckpoint.create(root / "result-release")) as result_release,
            closing(AllocationProfile(root)) as profile,
            McpClient(
                binary,
                DIRECT.serve("--control-socket", str(endpoint)),
                {
                    **os.environ,
                    **profile.environment,
                    "MCP_CONSOLE_TEST_RESULT_REACHED": str(result_reached.path),
                    "MCP_CONSOLE_TEST_RESULT_RELEASE": str(result_release.path),
                },
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.send(requirements={"r": ["later"]})
            # fmt: r
            r = code(r"""
                callback_gate <- tempfile("mcp-console-callback-gate-")
                callback_checkpoint <- tempfile("mcp-console-callback-checkpoint-")
                callback_release <- tempfile("mcp-console-callback-release-")
                run_callback <- function() {
                  if (!file.exists(callback_gate)) {
                    later::later(run_callback, delay = 0.01)
                    return(invisible(NULL))
                  }
                  cat("early idle\n")
                  stopifnot(file.create(callback_checkpoint))
                  while (!file.exists(callback_release)) {
                    Sys.sleep(0.01)
                  }
                  cat("late idle\n")
                }
                later::later(run_callback, delay = 0.01)
                cat(callback_gate, callback_checkpoint, callback_release, sep = "\n")
                """)
            client.send(r=r)
            (callback_release,) = release_worker_callback_gate(
                client, "blocked idle callback", ("release",)
            )
            wait_for_pending_text(endpoint, "early idle\n")
            # The worker reads the cell only after its callback returns. The
            # running response takes the early output; cancelling its delivery
            # returns that response to the evaluation.
            profile.pause_results(True)
            pending = client.start_send(r="print(42)", timeout_ms=10)
            cancel_result(client, pending, result_reached, result_release)
            profile.pause_results(False)
            callback_release.touch()
            wait_for_pending_text(endpoint, "[1] 42\n")
            with ControlClient(endpoint) as control:
                preview = result_text(control.request(op="preview"))
            # The preview follows MCP delivery order: the recovered response,
            # then output the worker wrote after it.
            assert preview.startswith("early idle\n"), preview
            assert preview.endswith("late idle\n[1] 42\n"), preview
            # MCP delivers the recovered response first, then the cell.
            client.expect(
                "early idle\n\n[running; poll with an empty send]", session="default"
            )
            client.expect(
                "late idle\n[output produced while idle]\n[1] 42\n", session="default"
            )
            return client.finish()


if __name__ == "__main__":
    run_this_suite(__file__)
