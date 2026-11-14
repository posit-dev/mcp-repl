"""Journal latency must not change the worker addressed by a standalone note."""

import os
import signal
import sys
import tempfile
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.recording.test_intervals import events
from support.allocations import AllocationProfile
from support.assertions import tool_text
from support.checkpoints import FifoCheckpoint
from support.client import McpClient
from support.execution import DIRECT
from support.native import LOADER_VARIABLE, build_interposer
from support.normalization import normalize_recording
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, requires


@requires(NATIVE_FIXTURES)
def test_standalone_note_keeps_received_worker_during_delayed_capture(
    binary: Path,
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    note = "The first worker has completed its baseline measurement."
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        with (
            closing(FifoCheckpoint.create(root / "observed")) as observed,
            closing(FifoCheckpoint.create(root / "release")) as release,
            closing(FifoCheckpoint.create(root / "dispatcher-exited")) as retired,
            closing(AllocationProfile(root)) as profile,
            McpClient(
                binary,
                DIRECT.serve("--worker", str(worker)),
                {
                    **os.environ,
                    **profile.environment,
                    LOADER_VARIABLE: os.pathsep.join(
                        [
                            profile.environment[LOADER_VARIABLE],
                            str(build_interposer(root, "recording_observation")),
                        ]
                    ),
                    "TMPDIR": str(root),
                    "ZOD_REPORT_PID": "1",
                    "MCP_CONSOLE_TEST_OBSERVED_EVENT": "tool_result",
                    "MCP_CONSOLE_TEST_JOURNAL_OBSERVED": str(observed.path),
                    "MCP_CONSOLE_TEST_JOURNAL_RELEASE": str(release.path),
                    "MCP_CONSOLE_TEST_DISPATCHER_EXITED": str(retired.path),
                    "MCP_CONSOLE_TEST_DISPATCHER_ARMED": str(root / "armed"),
                },
                root,
                # The stalled writer cannot record a delivery.
                recording_receipts=False,
            ) as client,
        ):
            try:
                client.initialize_and_list_tools()
                client.expect("zod: original\n", r="echo original")
                observed.wait("first result journal write is stalled", client=client)
                original_pid = int((root / "zod-worker-pid").read_text())
                # The note's reply follows its receipt and its queued capture.
                # Retirement follows both, while the writer has written neither.
                annotation = client.start_send(note=note)
                client.receive(annotation)
                assert tool_text(annotation["result"]) == "[done]", annotation
                (root / "armed").touch()
                os.kill(original_pid, signal.SIGKILL)
                retired.wait(
                    "worker retirement queued while the note is unwritten",
                    client=client,
                )
                release.release()
                client.recording_receipts = True
                failure = client.send()
                assert failure["isError"], failure
                client.expect(
                    "[starting new worker]\nzod: replacement\n",
                    r="echo replacement",
                )
                transcript = client.finish()
            finally:
                release.release()

        (recording,) = (root / ".agents/console/sessions").iterdir()
        capture = events(recording)
        received = next(
            event
            for event in capture
            if event["event"] == "protocol"
            and event["direction"] == "received"
            and event["message"].get("id") == annotation["id"]
        )
        retired = next(
            event
            for event in capture
            if event["event"] == "worker_ended" and event["session_id"] == 1
        )
        captured = next(
            event
            for event in capture
            if event["event"] == "tool_call"
            and event["request"]["arguments"].get("note") == note
        )
        assert received["sequence"] < captured["sequence"] < retired["sequence"], (
            capture
        )
        assert captured["note_target"] == {"kind": "worker", "session_id": 1}, captured
        original = (recording / "sessions/session-001.qmd").read_text()
        replacement = (recording / "sessions/session-002.qmd").read_text()
        assert original.count(note) == 1, original
        assert note not in replacement, replacement
        return normalize_recording(transcript, root)
