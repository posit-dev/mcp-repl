#!/usr/bin/env -S uv run --script

import json
import base64
import os
import sys
import tempfile
from contextlib import closing
from datetime import datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from support.checkpoints import (
    FifoCheckpoint,
)
from support.client import McpClient
from support.execution import DIRECT, SANDBOXED, Execution, executions
from support.native import LOADER_VARIABLE, build_interposer
from support.previews import live_recording_events, session_directory
from support.records import Transcript
from support.requirements import (
    NATIVE_FIXTURES,
    POSIX,
    R,
    SQL,
    command,
    requires,
)
from support.suites import run_this_suite
from boundaries.client_server.output.test_recovery import recovery_client


def recording_events(recording: Path) -> list[dict]:
    return [
        json.loads(line)
        for line in (recording / "capture/events.jsonl").read_text().splitlines()
    ]


@requires(NATIVE_FIXTURES)
def test_recording_send_keeps_following_cancellation_and_ping_live(
    binary: Path,
) -> Transcript:
    with recovery_client(binary) as (client, profile, reached, release):
        first = client.start_send(control="interrupt")
        reached.wait("first reply write starts", client=client)
        try:
            queued = client.start_send(r="echo cancelled cell must not run")
            controlled = client.start_send(
                control="restart", r="echo cancelled restart must not run"
            )
            client.notify("notifications/cancelled", requestId=queued["id"])
            client.notify("notifications/cancelled", requestId=controlled["id"])
            # The held write blocks every later reply. The recorded receipt of
            # this ping proves that the server applied both cancellations.
            ping = client.start_request("ping")
            client.await_received(ping)
        finally:
            profile.pause_results(False)
            release.release()
        client.receive(first)
        assert first["result"] == {
            "content": [{"type": "text", "text": "\n[idle]"}],
            "isError": False,
        }, first
        client.receive(ping)
        assert ping["result"] == {}, ping
        client.expect("zod: after\n", r="echo after")
        recording = session_directory(client)
        transcript = client.transcript
        events = live_recording_events(recording)
        calls = [event for event in events if event["event"] == "tool_call"]
        assert [event["request_id"] for event in calls] == [
            entry["id"] for entry in transcript if "send" in entry
        ]
        assert (
            len([event for event in events if event["event"] == "worker_started"]) == 1
        )
        assert (
            recording / "capture/sessions/001/cell-002.txt"
        ).read_text() == "zod: after\n"
        assert [
            event["call_id"] for event in events if event["event"] == "cell_started"
        ] == [1, 5]
        assert not (recording / "capture/sessions/001/cell-003.txt").exists()
        received = [
            event["message"]
            for event in events
            if event["event"] == "protocol" and event["direction"] == "received"
        ]
        positions = {
            message["id"]: i for i, message in enumerate(received) if "id" in message
        }
        assert positions[queued["id"]] < positions[controlled["id"]]
        cancellations = [
            message["params"]["requestId"]
            for message in received
            if message.get("method") == "notifications/cancelled"
        ]
        assert cancellations == [queued["id"], controlled["id"]]
        cancelled = [
            event["error"]["message"]
            for event in events
            if event["event"] == "tool_result" and event["call_id"] in (3, 4)
        ]
        assert cancelled == ["request cancelled before execution"] * 2, cancelled
        return client.finish()


@requires(NATIVE_FIXTURES, R, SQL, command("ir"))
@executions(DIRECT)
def test_first_rejected_send_publishes_the_observed_prelude(
    binary: Path, execution: Execution
) -> Transcript:
    from support.r import r_test_environment, reference_plots
    from boundaries.client_server.recording.test_journal import (
        STARTUP_PLOT,
        configure_r_startup,
    )

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        environment, rscript = r_test_environment()
        (expected_image,) = reference_plots(
            rscript, environment, STARTUP_PLOT, width=4, height=3, dpi=100, pages=1
        )
        configure_r_startup(
            root,
            environment,
            rscript,
            'cat("retained startup text\\n")\n' + STARTUP_PLOT,
        )
        with closing(FifoCheckpoint.create(root / "observed")) as observed:
            environment.update(
                {
                    "RETICULATE_PYTHON": sys.executable,
                    LOADER_VARIABLE: str(
                        build_interposer(root, "recording_observation")
                    ),
                    "MCP_CONSOLE_TEST_OBSERVED_EVENT": "image_retained",
                    "MCP_CONSOLE_TEST_JOURNAL_OBSERVED": str(observed.path),
                }
            )
            with McpClient(binary, execution.serve(), environment, root) as client:
                client.initialize_and_list_tools()
                observed.wait(
                    "server recorded startup text and image",
                    timeout=600,
                    client=client,
                )
                assert client.request("ping")["result"] == {}
                assert not (root / ".agents/console/sessions").exists()
                before_send = datetime.now().astimezone()
                rejected = client.send(r="must not execute", python="must not execute")
                assert rejected["isError"] is True
                (recording,) = (root / ".agents/console/sessions").iterdir()
                published = live_recording_events(recording)
                prelude = [
                    e
                    for e in published
                    if e["event"]
                    in {
                        "recording_started",
                        "worker_started",
                        "worker_ready",
                        "image_retained",
                    }
                ]
                assert [e["event"] for e in prelude] == [
                    "recording_started",
                    "worker_started",
                    "worker_ready",
                    "image_retained",
                ]
                assert all(
                    datetime.fromisoformat(e["at"]) < before_send for e in prelude
                )
                activation = next(
                    e for e in published if e["event"] == "recording_activated"
                )
                assert datetime.fromisoformat(activation["at"]) >= before_send
                assert (
                    recording / "capture/sessions/001/startup.txt"
                ).read_text() == "retained startup text\n"
                image = next(e for e in prelude if e["event"] == "image_retained")
                assert (recording / image["path"]).read_bytes() == expected_image
                assert not client.send(session="default")["isError"]
                assert not client.send(stdin="unused input")["isError"]
                transcript = client.finish()
            events = recording_events(recording)
            assert [e["sequence"] for e in events] == list(range(1, len(events) + 1))
            received = [
                e["message"]
                for e in events
                if e["event"] == "protocol" and e["direction"] == "received"
            ]
            assert [m["method"] for m in received] == [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "ping",
                "tools/call",
                "tools/call",
                "tools/call",
            ]
            assert [m["params"] for m in received if m["method"] == "tools/call"] == [
                {"tool_call_id": i} for i in range(1, 4)
            ]
            assembled = [
                e["message"]
                for e in events
                if e["event"] == "protocol" and e["direction"] == "assembled"
            ]
            assert [
                m["result"]
                for m in assembled
                if m.get("id")
                in {e["request_id"] for e in events if e["event"] == "tool_call"}
            ] == [{"tool_result_call_id": i} for i in range(1, 4)]
            assert events[-1]["event"] == "recording_ended"
            assert all(e["delivered"] for e in events if e["event"] == "delivery")
            for entry in transcript:
                for block in entry.get("result", {}).get("content", []):
                    if block["type"] == "image":
                        assert base64.b64decode(block["data"]) == expected_image
                        block["data"] = "<verified startup PNG>"
            return transcript


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_sessions_follow_actual_worker_lifetimes(
    binary: Path, execution: Execution
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        control = root / "startup-control"
        control.write_text("ready")
        with McpClient(
            binary,
            execution.serve("--worker", str(worker)),
            {**os.environ, "ZOD_STARTUP_CONTROL": str(control)},
            root,
        ) as client:
            client.initialize_and_list_tools()
            client.send(r="echo first")
            client.send(control="interrupt")
            client.send(control="restart", requirements={"action": "get"})
            client.send(r="echo still first")
            (recording,) = (root / ".agents/console/sessions").iterdir()
            assert (
                len(
                    [
                        e
                        for e in live_recording_events(recording)
                        if e["event"] == "worker_started"
                    ]
                )
                == 1
            )
            client.send(control="restart")
            client.send(r="echo second")
            client.send(r="exit unexpectedly")
            client.send(r="echo third")
            client.finish()
        events = recording_events(recording)
        for transition in ("worker_started", "worker_ready", "worker_ended"):
            assert [e["session_id"] for e in events if e["event"] == transition] == [
                1,
                2,
                3,
            ], (transition, events)
        assert not any(e["event"] == "startup_failed" for e in events)
        assert len(list((root / ".agents/console/sessions").iterdir())) == 1
        return client.transcript


if __name__ == "__main__":
    run_this_suite(__file__)
