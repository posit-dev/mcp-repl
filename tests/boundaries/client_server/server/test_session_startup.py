"""Connection closure owns pending discovery for every logical session."""

import json
import sys
import tempfile
from contextlib import ExitStack, closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from support.checkpoints import FifoCheckpoint
from support.client import McpClient
from support.native import LOADER_VARIABLE, build_interposer
from support.discovery import discovery_environment
from support.execution import DIRECT
from support.normalization import normalize_process_diagnostic, normalize_recording
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, PROCESS_EVENTS, command, requires
from support.suites import run_this_suite
from boundaries.client_server.recording.test_boundaries import await_event
from boundaries.client_server.server.test_startup import wait_for_send_admission


@requires(command("rustc"))
def test_closed_input_cancels_named_discovery(binary: Path) -> Transcript:
    with discovery_environment() as (environment, discovery):
        hidden = discovery.executable.with_name("hidden-R")
        discovery.executable.rename(hidden)
        environment["RETICULATE_PYTHON"] = sys.executable
        with McpClient(binary, DIRECT.serve(), environment) as client:
            client.initialize_and_list_tools()
            client.expect("42\n", python="42")
            hidden.rename(discovery.executable)
            pending = client.start_send(
                session="alpha", python="raise AssertionError('cell executed')"
            )
            discovery.wait("named runtime discovery is blocked after default is ready")
            client.stdin.close()
            client.receive(pending)
            assert pending["result"]["isError"] is True, pending
            assert "cell executed" not in str(pending["result"]), pending
            transcript = client.finish()
        return json.loads(
            normalize_process_diagnostic(json.dumps(transcript)).replace(
                str(discovery.root), "<discovery>"
            )
        )


@requires(NATIVE_FIXTURES, PROCESS_EVENTS, command("rustc"))
def test_cancelled_aggregate_poll_releases_named_startup_observation(
    binary: Path,
) -> Transcript:
    with discovery_environment() as (environment, discovery), ExitStack() as resources:
        root = discovery.root
        hidden = discovery.executable.with_name("hidden-R")
        discovery.executable.rename(hidden)
        environment["RETICULATE_PYTHON"] = sys.executable
        write_reached = resources.enter_context(
            closing(FifoCheckpoint.create(root / "write-reached"))
        )
        write_release = resources.enter_context(
            closing(FifoCheckpoint.create(root / "write-release"))
        )
        armed = root / "write-armed"
        # The discovery root contains spaces, which Linux LD_PRELOAD separates.
        interposers = Path(resources.enter_context(tempfile.TemporaryDirectory()))
        environment.update(
            {
                LOADER_VARIABLE: str(
                    build_interposer(interposers, "response_write_interposer")
                ),
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_REACHED": str(write_reached.path),
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_RELEASE": str(write_release.path),
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_MATCH": "[running; poll with an empty send]",
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_COMPLETE": "1",
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_ARMED": str(armed),
            }
        )
        with McpClient(binary, DIRECT.serve(), environment, root) as client:
            client.initialize_and_list_tools()
            client.expect("42\n", python="42")
            hidden.rename(discovery.executable)
            accepted = client.send(session="alpha", python="42", timeout_ms=0)
            assert accepted["isError"] is False, accepted
            discovery.wait("named discovery stays blocked throughout cancellation")
            try:
                for selection in ({"session": "alpha"}, {}):
                    previous_id = client.transcript[-1]["id"]
                    if not selection:
                        write_reached.wait(
                            "complete prior response has an unsettled write"
                        )
                    # The serialized ping write follows the previous response's
                    # settlement. A tool poll sent earlier could queue behind the
                    # long poll whose observation claim it is meant to inspect.
                    barrier = client.start_request("ping")
                    if not selection:
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
                        lambda event: event["request_id"] == previous_id,
                    )
                    assert delivered["delivered"] is True, delivered
                    pending = client.start_send(**selection, timeout_ms=60_000)
                    wait_for_send_admission(client, session="alpha")
                    client.notify("notifications/cancelled", requestId=pending["id"])
                    assert client.request("ping")["result"] == {}
                    call = await_event(
                        client,
                        root,
                        "tool_call",
                        lambda event: event["request_id"] == pending["id"],
                    )
                    # The journal confirms handler cancellation while discovery is
                    # still blocked, before the next poll attempts the released claim.
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
                    if selection:
                        armed.touch()
                        # The held write returns only on release, so the recording
                        # cannot yet hold this reply's delivery.
                        client.recording_receipts = False
                    observed = client.send(session="alpha", timeout_ms=0)
                    client.recording_receipts = True
                    assert observed["isError"] is False, observed
                    assert observed["content"][0]["text"].endswith(
                        "[running; poll with an empty send]"
                    ), observed
            finally:
                write_release.release()
            return normalize_recording(client.finish(), root)


if __name__ == "__main__":
    run_this_suite(__file__)
