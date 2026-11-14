"""Interrupt signaling preserves another send's output ownership."""

import os
import sys
import tempfile
from contextlib import ExitStack, closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from support.progress import elapsed_progress, without_elapsed, without_elapsed_result
from support.assertions import last_tool_text
from support.checkpoints import FifoCheckpoint, wait_for_worker_file
from support.client import McpClient
from support.execution import DIRECT, SANDBOXED, Execution, executions
from support.native import LOADER_VARIABLE, build_interposer
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, PROCESS_EVENTS, requires
from support.suites import run_this_suite

from boundaries.client_server._harness import ZodFixtureControl


@executions(DIRECT, SANDBOXED)
@requires(PROCESS_EVENTS)
def test_interrupt_preserves_active_poll_output(
    binary: Path, execution: Execution
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with ZodFixtureControl() as control:
        environment = os.environ.copy()
        control.configure(environment)
        with McpClient(
            binary, execution.serve("--worker", str(worker)), environment
        ) as client:
            client.initialize_and_list_tools()
            # Finish shared discovery and lazy worker startup before polling.
            client.send(requirements={"action": "get"})
            client.send(control="restart")
            assert last_tool_text(client) == "[starting new worker]\n[idle]"
            client.send(r="shutdown output checkpoints", timeout_ms=0)
            assert (
                without_elapsed(last_tool_text(client))
                == "\n[running; poll with an empty send]"
            )
            control.connect(client)
            control.wait_for(0, "evaluation_started")
            control.send_control(0, "emit_output")
            control.wait_for(0, "output_processed")
            waiting = client.start_send(stdin="p", timeout_ms=40_000)
            control.send_control(0, "observe_poll_ownership", request=waiting["id"])
            control.wait_for(waiting["id"], "poll_ownership_observed")
            try:
                client.send(session="default", timeout_ms=0)
                assert client.transcript[-1]["result"] == {
                    "content": [
                        {
                            "type": "text",
                            "text": "[worker evaluation is already being polled]",
                        }
                    ],
                    "isError": True,
                }
                client.send(control="interrupt", timeout_ms=0)
                assert elapsed_progress(last_tool_text(client))[1] is False
                assert without_elapsed_result(client.transcript[-1]["result"]) == {
                    "content": [
                        {"type": "text", "text": "\n[running; poll with an empty send]"}
                    ],
                    "isError": False,
                }, client.transcript[-1]
                wait_for_worker_file(control.root, "zod-sigint-received", client)
            finally:
                control.send_control(0, "complete")
            control.wait_for(0, "completion_processed")
            client.receive(waiting)
            content = waiting["result"]["content"]
            assert content[0] == {"type": "text", "text": "before shutdown\n"}
            assert content[1]["type"] == "image"
            assert content[2] == {"type": "text", "text": "after image\n"}
            assert not waiting["result"]["isError"], waiting
            client.send(session="default", timeout_ms=0)
            assert last_tool_text(client) == "\n[idle]", client.transcript[-1]
            client.send(r="echo next cell")
            assert last_tool_text(client) == "zod: next cell\n"
            return client.finish()


@executions(DIRECT, SANDBOXED)
@requires(NATIVE_FIXTURES, PROCESS_EVENTS)
def test_interrupt_preserves_completed_response_delivery(
    binary: Path, execution: Execution
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as temporary, ExitStack() as resources:
        root = Path(temporary)
        (
            grace_reached,
            grace_release,
            write_reached,
            write_release,
        ) = [
            resources.enter_context(closing(FifoCheckpoint.create(root / name)))
            for name in (
                "grace-reached",
                "grace-release",
                "write-reached",
                "write-release",
            )
        ]
        environment = {
            **os.environ,
            "MCP_CONSOLE_TEST_INTERRUPT_GRACE_REACHED": str(grace_reached.path),
            "MCP_CONSOLE_TEST_INTERRUPT_GRACE_RELEASE": str(grace_release.path),
            "MCP_CONSOLE_TEST_RESPONSE_WRITE_REACHED": str(write_reached.path),
            "MCP_CONSOLE_TEST_RESPONSE_WRITE_RELEASE": str(write_release.path),
            "MCP_CONSOLE_TEST_RESPONSE_WRITE_MATCH": "before shutdown",
            "MCP_CONSOLE_TEST_RESPONSE_WRITE_COMPLETE": "1",
        }
        environment[LOADER_VARIABLE] = ":".join(
            str(build_interposer(root, name))
            for name in ("response_write_interposer", "interrupt_grace_checkpoint")
        )
        with ZodFixtureControl(root) as control:
            control.configure(environment)
            with McpClient(
                binary, execution.serve("--worker", str(worker)), environment
            ) as client:
                client.initialize_and_list_tools()
                # Finish shared discovery and lazy worker startup before polling.
                client.send(requirements={"action": "get"})
                client.send(control="restart")
                assert last_tool_text(client) == "[starting new worker]\n[idle]"
                client.send(r="shutdown output checkpoints", timeout_ms=0)
                control.connect(client)
                control.wait_for(0, "evaluation_started")
                control.send_control(0, "emit_output")
                control.wait_for(0, "output_processed")
                waiting = client.start_send(stdin="p", timeout_ms=40_000)
                control.send_control(0, "observe_poll_ownership", request=waiting["id"])
                control.wait_for(waiting["id"], "poll_ownership_observed")
                try:
                    interrupt = client.start_send(control="interrupt", timeout_ms=0)
                    grace_reached.wait("interrupt admitted and signal acknowledged")
                    wait_for_worker_file(root, "zod-sigint-received", client)
                    control.send_control(0, "complete")
                    control.wait_for(0, "completion_processed")
                    write_reached.wait(
                        "original response visible with delivery unsettled"
                    )
                    # The held write returns only on release, so the recording
                    # cannot yet hold this reply's delivery.
                    client.recording_receipts = False
                    client.receive(waiting)
                    client.recording_receipts = True
                    content = waiting["result"]["content"]
                    assert content[0] == {"type": "text", "text": "before shutdown\n"}
                    assert content[1]["type"] == "image"
                    assert content[2] == {"type": "text", "text": "after image\n"}
                    assert not waiting["result"]["isError"], waiting
                    grace_release.release()
                    # Control must finish observing while the original response
                    # still owns delivery, without waiting for its acknowledgment.
                    client.await_assembled(interrupt)
                finally:
                    grace_release.release()
                    write_release.release()
                client.receive(interrupt)
                assert without_elapsed_result(interrupt["result"]) == {
                    "content": [
                        {"type": "text", "text": "\n[running; poll with an empty send]"}
                    ],
                    "isError": False,
                }, interrupt
                client.send(session="default", timeout_ms=0)
                assert last_tool_text(client) == "\n[idle]", client.transcript[-1]
                client.send(r="echo after completed interrupt")
                assert last_tool_text(client) == "zod: after completed interrupt\n"
                return client.finish()


if __name__ == "__main__":
    run_this_suite(__file__)
