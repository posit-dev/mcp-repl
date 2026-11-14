#!/usr/bin/env -S uv run --script

import os
import sys
import tempfile
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from support.allocations import AllocationProfile
from support.assertions import last_tool_text
from support.checkpoints import FifoCheckpoint, wait_for_worker_file
from support.client import McpClient
from support.events import Events
from support.execution import DIRECT, SANDBOXED, Execution, executions
from support.processes import host_process_id
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, PROCESS_EVENTS, requires
from support.suites import run_this_suite


@executions(DIRECT, SANDBOXED)
@requires(PROCESS_EVENTS)
def test_cancelled_result_is_not_written_after_eof(
    binary: Path, execution: Execution
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with (
            Events() as exits,
            McpClient(
                binary,
                execution.serve("--worker", str(worker)),
                {**os.environ, "TMPDIR": str(root), "ZOD_REPORT_PID": "1"},
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.send(r="echo ready")
            worker_pid = host_process_id(
                int(wait_for_worker_file(root, "zod-worker-pid", client).read_text()),
                client.process.pid,
            )
            exits.watch_process(worker_pid)
            pending = client.start_send(r="complete after release")
            wait_for_worker_file(root, "zod-evaluation-started", client)
            client.notify("notifications/cancelled", requestId=pending["id"])
            assert client.request("ping")["result"] == {}
            client.stdin.close()
            # The cancelled handler still owns the running cell. Worker
            # retirement ends it only after the server observes MCP EOF, so its
            # result reaches the SDK's EOF drain rather than the live loop.
            assert exits.wait(5) == {worker_pid}
            # Finishing fails on any output that follows the ping reply.
            transcript = client.finish()
            assert pending.keys() == {"id", "send"}, pending
            return transcript


@executions(DIRECT, SANDBOXED)
@requires(NATIVE_FIXTURES, PROCESS_EVENTS)
def test_accepted_result_is_written_after_eof(
    binary: Path, execution: Execution
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with (
            closing(FifoCheckpoint.create(root / "result-reached")) as reached,
            closing(FifoCheckpoint.create(root / "result-release")) as release,
            closing(AllocationProfile(root)) as profile,
            Events() as exits,
            McpClient(
                binary,
                execution.serve("--worker", str(worker)),
                {
                    **os.environ,
                    **profile.environment,
                    "TMPDIR": str(root),
                    "ZOD_REPORT_PID": "1",
                    "MCP_CONSOLE_TEST_RESULT_REACHED": str(reached.path),
                    "MCP_CONSOLE_TEST_RESULT_RELEASE": str(release.path),
                },
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.send(r="echo ready")
            worker_pid = host_process_id(
                int(wait_for_worker_file(root, "zod-worker-pid", client).read_text()),
                client.process.pid,
            )
            exits.watch_process(worker_pid)
            profile.pause_results(True)
            try:
                pending = client.start_send(r="echo held response")
                reached.wait("reply write starts", client=client)
                client.stdin.close()
                # Worker retirement proves the server observed MCP EOF while
                # the write of this reply was still held.
                assert exits.wait(5) == {worker_pid}
            finally:
                profile.pause_results(False)
                release.release()
            client.receive(pending)
            assert last_tool_text(client) == "zod: held response\n"
            return client.finish()


if __name__ == "__main__":
    run_this_suite(__file__)
