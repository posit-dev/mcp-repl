"""An integrator subscribes once to pending output without consuming it."""

import json
import sys
import tempfile
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.integrations.test_control import (
    ControlClient,
    result_text,
)
from boundaries.client_server.output.test_recovery import cancel_held_result
from boundaries.client_server.python.test_without_r import selected_environment
from support.allocations import AllocationProfile
from support.checkpoints import FifoCheckpoint
from support.client import McpClient
from support.execution import DIRECT, SANDBOXED, Execution, executions
from support.normalization import code, normalize_recording
from support.records import TranscriptWithCompanions
from support.requirements import NATIVE_FIXTURES, POSIX, requires
from support.suites import run_this_suite


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_pushes_pending_output_and_clears_after_collection(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with (
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
            with (
                ControlClient(endpoint) as observer,
                ControlClient(endpoint) as inspector,
            ):
                subscribed = observer.request(op="subscribe")
                assert subscribed == {"id": "1", "result": {"subscribed": True}}, (
                    subscribed
                )

                def preview() -> dict:
                    event = json.loads(observer.reader.readline(timeout=30))
                    assert event["id"] == "1" and event["event"] == "preview", event
                    return event

                initial = preview()
                assert result_text(initial) == "", initial
                # fmt: python
                source = code(f"""
                    print("pending ghost", end="", flush=True)
                    with open({str(release.path)!r}, "rb", buffering=0) as gate:
                        gate.read(1)
                    print(" complete", flush=True)
                    """)
                pending = client.start_send(python=source, timeout_ms=60000)
                try:
                    update = preview()
                    while result_text(update) != "pending ghost":
                        update = preview()
                    inspected = inspector.request(op="preview")
                    assert result_text(inspected) == "pending ghost", inspected
                    with ControlClient(endpoint) as late:
                        late.request(op="subscribe")
                        replay = json.loads(late.reader.readline(timeout=30))
                        assert replay["event"] == "preview", replay
                        assert result_text(replay) == "pending ghost", replay
                    release.release()
                    client.receive(pending)
                    text = "".join(
                        block.get("text", "") for block in pending["result"]["content"]
                    )
                    assert text == "pending ghost complete\n", pending
                    cleared = preview()
                    while result_text(cleared):
                        cleared = preview()
                    client.expect("\n[idle]")
                    transcript = normalize_recording(client.finish(), root)
                finally:
                    release.release()
            return TranscriptWithCompanions(
                transcript, {"stream.yaml": [initial, update, cleared]}
            )


@requires(POSIX, NATIVE_FIXTURES)
@executions(DIRECT)
def test_pushes_cancelled_response_recovery_without_polling(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with (
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
                    "scenario": "A subscription restores ghost output after an MCP response is cancelled, without another status or preview request.",
                    "fixture": "A worker FIFO holds output pending; a native checkpoint then holds the write of its assembled MCP response while the client cancels delivery.",
                }
            )
            client.initialize_and_list_tools()
            client.expect(python="pass")
            with ControlClient(endpoint) as observer:
                observer.request(op="subscribe")

                def preview() -> dict:
                    event = json.loads(observer.reader.readline(timeout=10))
                    assert event["id"] == "1" and event["event"] == "preview", event
                    return event

                initial = preview()
                assert result_text(initial) == "", initial
                # fmt: python
                source = code(f"""
                    print("recovered ghost", flush=True)
                    with open({str(release.path)!r}, "rb", buffering=0) as gate:
                        gate.read(1)
                    """)
                profile.pause_results(True)
                pending = client.start_send(python=source, timeout_ms=60000)
                try:
                    visible = preview()
                    while result_text(visible) != "recovered ghost\n":
                        visible = preview()
                    release.release()
                    result_reached.wait(
                        "MCP response owns pending output", client=client
                    )
                    claimed = preview()
                    while result_text(claimed):
                        claimed = preview()
                    cancel_held_result(client, pending, result_release)
                    profile.pause_results(False)
                    recovered = preview()
                    assert result_text(recovered) == "recovered ghost\n", recovered
                    client.expect("recovered ghost\n")
                    client.expect("\n[idle]")
                    transcript = normalize_recording(client.finish(), root)
                finally:
                    profile.pause_results(False)
                    release.release()
                    result_release.release()
            return TranscriptWithCompanions(
                transcript, {"stream.yaml": [initial, visible, claimed, recovered]}
            )


if __name__ == "__main__":
    run_this_suite(__file__)
