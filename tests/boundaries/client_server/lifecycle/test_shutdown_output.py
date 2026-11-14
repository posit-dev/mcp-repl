#!/usr/bin/env -S uv run --script

import os
import sys
import tempfile
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from support.progress import without_elapsed
from support.assertions import last_tool_text
from support.allocations import AllocationProfile
from support.checkpoints import FifoCheckpoint
from support.client import McpClient, stop_client
from support.execution import DIRECT, SANDBOXED, Execution, executions
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, PROCESS_EVENTS, requires
from support.suites import run_this_suite

from boundaries.client_server._harness import ZodFixtureControl


def _shutdown_with_collected_output(
    binary: Path, execution: Execution, *, completed: bool
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    environment = os.environ.copy()
    with ZodFixtureControl() as control:
        control.configure(environment)
        client = McpClient(
            binary,
            execution.serve("--worker", str(zod)),
            environment,
        )
        try:
            client.initialize_and_list_tools()
            # Finish shared discovery and lazy worker startup before testing EOF.
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
            ownership = control.wait_for(waiting["id"], "poll_ownership_observed")
            assert ownership["target_operation"] == 0, ownership
            client.send(session="default", timeout_ms=0)
            assert client.transcript[-1]["result"] == {
                "content": [
                    {
                        "type": "text",
                        "text": "[worker evaluation is already being polled]",
                    }
                ],
                "isError": True,
            }, client.transcript[-1]
            if completed:
                control.send_control(0, "complete")
                control.wait_for(0, "completion_processed")

            client.stdin.close()
            return_code = client.process.wait(timeout=3)
            client.receive(waiting)
            result = waiting["result"]
            content = result["content"]
            assert content[0] == {"type": "text", "text": "before shutdown\n"}, result
            assert content[1]["type"] == "image", result
            assert content[2]["type"] == "text", result
            assert content[2]["text"].startswith("after image\n"), result
            assert return_code == 0, client.stderr.read()
            assert client.stdout.read() == ""
            assert client.stderr.read() == ""
            return client.transcript
        finally:
            stop_client(client)


@executions(DIRECT, SANDBOXED)
@requires(PROCESS_EVENTS)
def test_eof_preserves_output_from_active_cell(
    binary: Path, execution: Execution
) -> Transcript:
    return _shutdown_with_collected_output(binary, execution, completed=False)


@executions(DIRECT, SANDBOXED)
@requires(PROCESS_EVENTS)
def test_eof_preserves_output_after_cell_completion(
    binary: Path, execution: Execution
) -> Transcript:
    return _shutdown_with_collected_output(binary, execution, completed=True)


@executions(DIRECT, SANDBOXED)
@requires(NATIVE_FIXTURES)
def test_eof_preserves_first_send_response(
    binary: Path, execution: Execution
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with (
            closing(FifoCheckpoint.create(root / "result-reached")) as reached,
            closing(FifoCheckpoint.create(root / "result-release")) as release,
            closing(AllocationProfile(root)) as profile,
            McpClient(
                binary,
                execution.serve("--worker", str(zod)),
                {
                    **os.environ,
                    **profile.environment,
                    "MCP_CONSOLE_TEST_RESULT_REACHED": str(reached.path),
                    "MCP_CONSOLE_TEST_RESULT_RELEASE": str(release.path),
                },
            ) as client,
        ):
            client.initialize_and_list_tools()
            profile.pause_results(True)
            try:
                waiting = client.start_send(r="1", python="1")
                # Hold the write of the accepted reply. Writing stdin alone
                # does not establish acceptance before EOF.
                reached.wait("first send owns response delivery", timeout=30)
                client.stdin.close()
            finally:
                profile.pause_results(False)
                release.release()
            assert client.process.wait(timeout=client.shutdown_timeout) == 0, (
                client.stderr.read()
            )
            client.receive(waiting)
            assert waiting["result"] == {
                "content": [
                    {
                        "type": "text",
                        "text": "only one of `r`, `python`, or `sql` may be supplied",
                    }
                ],
                "isError": True,
            }, waiting
            assert client.stdout.read() == ""
            assert client.stderr.read() == ""
            return client.transcript


if __name__ == "__main__":
    run_this_suite(__file__)
