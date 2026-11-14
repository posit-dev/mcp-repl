#!/usr/bin/env -S uv run --script

import os
import sys
import tempfile
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.recording.test_process import recording_events
from boundaries.client_server.output.test_recovery import recovery_client
from support.allocations import RetentionProfile
from support.checkpoints import FifoCheckpoint
from support.client import McpClient
from support.execution import DIRECT, Execution, executions
from support.native import LOADER_VARIABLE, build_interposer
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, POSIX, requires
from support.suites import run_this_suite


@requires(NATIVE_FIXTURES)
def test_queued_calls_keep_their_response_identity(
    binary: Path,
) -> Transcript:
    with (
        tempfile.TemporaryDirectory() as directory,
        recovery_client(binary, recording_directory=Path(directory)) as (
            client,
            profile,
            reached,
            release,
        ),
    ):
        first = client.start_send(session="default")
        try:
            reached.wait("first reply write starts", client=client)
            # The later request is received while the write of the earlier
            # reply is held. MCP request IDs remain unique throughout the session.
            second = client.start_send(r="echo queued")
            ping = client.start_request("ping")
            client.await_received(ping)
        finally:
            profile.pause_results(False)
            release.release()
        client.receive_many([first, ping, second])
        assert ping["result"] == {}, ping
        assert first["result"] == {
            "content": [{"type": "text", "text": "\n[idle]"}],
            "isError": False,
        }, first
        assert second["result"] == {
            "content": [{"type": "text", "text": "zod: queued\n"}],
            "isError": False,
        }, second
        client.finish()
        (recording,) = (Path(directory) / "sessions").iterdir()
        events = recording_events(recording)
        received = [
            e
            for e in events
            if e["event"] == "protocol"
            and e["direction"] == "received"
            and e["message"].get("method") == "tools/call"
        ]
        assert [e["message"]["params"] for e in received] == [
            {"tool_call_id": 1},
            {"tool_call_id": 2},
            {"tool_call_id": 3},
        ]
        calls = [e for e in events if e["event"] == "tool_call"]
        assert [e["request_id"] for e in calls] == [
            e["id"] for e in client.transcript if "send" in e
        ]
        for frame, call in zip(received, calls, strict=True):
            assert frame["sequence"] < call["sequence"]
        (activation,) = [e for e in events if e["event"] == "recording_activated"]
        assert received[0]["sequence"] < activation["sequence"] < calls[0]["sequence"]
        request_ids = {first["id"], second["id"]}
        assembled = [
            e
            for e in events
            if e["event"] == "protocol"
            and e["direction"] == "assembled"
            and e["message"].get("id") in request_ids
        ]
        delivered = [
            e
            for e in events
            if e["event"] == "delivery" and e["request_id"] in request_ids
        ]
        assert len(assembled) == len(delivered) == 2, (assembled, delivered)
        for call_id, request in ((2, first), (3, second)):
            (response,) = [e for e in assembled if e["message"]["id"] == request["id"]]
            assert response["message"] == {
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"tool_result_call_id": call_id},
            }, response
            (delivery,) = [e for e in delivered if e["request_id"] == request["id"]]
            assert delivery["call_id"] == call_id and delivery["delivered"], delivery
            (result,) = [
                e
                for e in events
                if e["event"] == "tool_result" and e["call_id"] == call_id
            ]
            assert (
                calls[call_id - 1]["sequence"]
                < result["sequence"]
                < response["sequence"]
                < delivery["sequence"]
            ), (result, response, delivery)
        assert events[-1]["event"] == "recording_ended"
        return [
            *client.transcript[:3],
            {"response identity": "distinct queued calls"},
        ]


def protocol_backlog(binary: Path, *, large_frames: bool) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        with (
            closing(FifoCheckpoint.create(root / "observed")) as observed,
            closing(FifoCheckpoint.create(root / "release")) as release,
            closing(RetentionProfile(root)) as profile,
        ):
            environment = {
                **os.environ,
                **profile.environment,
                LOADER_VARIABLE: os.pathsep.join(
                    [
                        profile.environment[LOADER_VARIABLE],
                        str(build_interposer(root, "recording_observation")),
                    ]
                ),
                "MCP_CONSOLE_TEST_OBSERVED_EVENT": "tool_result"
                if large_frames
                else "protocol",
                "MCP_CONSOLE_TEST_JOURNAL_OBSERVED": str(observed.path),
                "MCP_CONSOLE_TEST_JOURNAL_RELEASE": str(release.path),
            }
            with McpClient(
                binary,
                DIRECT.serve("--worker", str(worker)),
                environment,
                root,
                response_timeout=15,
                # The stalled writer cannot record a delivery.
                recording_receipts=False,
            ) as client:
                try:
                    client.initialize_and_list_tools()
                    pending = (
                        client.start_send(r="echo ready") if large_frames else None
                    )
                    observed.wait("protocol journal writer stalled", client=client)
                    if pending is not None:
                        # The reply does not wait for its stalled journal write.
                        client.receive(pending)
                        assert pending["result"] == {
                            "content": [{"type": "text", "text": "zod: ready\n"}],
                            "isError": False,
                        }, pending
                    baseline = profile.live
                    # Past 64 queued frames or 16 MiB, Console stops reading
                    # requests until the writer catches up. This writer never
                    # does: after 5 seconds without progress, recording stops
                    # and the remaining requests are read without it.
                    for _ in range(96 if large_frames else 400):
                        client.notify(
                            "notifications/cancelled",
                            requestId=1_000_000,
                            reason="x" * (1024 * 1024 if large_frames else 1),
                        )
                        assert client.request("ping")["result"] == {}
                        # The paced byte bound, the frame that crosses it, and
                        # the frame being read.
                        assert profile.live - baseline < 24 * 1024 * 1024, (
                            profile.live - baseline
                        )
                finally:
                    release.release()
                # The writer reports the failure once its held write returns.
                client.await_recording_disabled()
                client.expect("zod: after overflow\n", r="echo after overflow")
                _, diagnostics = client.finish_with_standard_error()
                assert diagnostics == (
                    "mcp-console: transcript recording disabled: "
                    "recording writer made no progress for 5 seconds\n"
                ), diagnostics
                if large_frames:
                    (recording,) = (root / ".agents/console/sessions").iterdir()
                    events = recording_events(recording)
                    assert [
                        e["call_id"] for e in events if e["event"] == "tool_call"
                    ] == [1]
                    assert not any(e["event"] == "recording_ended" for e in events)
                    # Capture failure does not keep completed managed files writable.
                    for path in recording.rglob("*"):
                        if path.is_file():
                            assert path.stat().st_mode & 0o222 == 0, path

                else:
                    assert not (root / ".agents/console/sessions").exists()
                return [
                    *client.transcript[:3],
                    {
                        "protocol backlog": "bounded; computation continues",
                        "stderr": diagnostics,
                    },
                ]


@requires(NATIVE_FIXTURES)
def test_protocol_backlog_has_a_byte_bound(binary: Path) -> Transcript:
    return protocol_backlog(binary, large_frames=True)


@requires(NATIVE_FIXTURES)
def test_protocol_backlog_has_a_frame_bound(binary: Path) -> Transcript:
    return protocol_backlog(binary, large_frames=False)


@requires(NATIVE_FIXTURES)
def test_protocol_writer_does_not_hold_frame_reception(binary: Path) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        with (
            closing(FifoCheckpoint.create(root / "observed")) as observed,
            closing(FifoCheckpoint.create(root / "release")) as release,
        ):
            environment = {
                **os.environ,
                LOADER_VARIABLE: str(build_interposer(root, "recording_observation")),
                "MCP_CONSOLE_TEST_OBSERVED_EVENT": "protocol",
                "MCP_CONSOLE_TEST_JOURNAL_OBSERVED": str(observed.path),
                "MCP_CONSOLE_TEST_JOURNAL_RELEASE": str(release.path),
            }
            with McpClient(
                binary,
                DIRECT.serve("--worker", str(worker)),
                environment,
                root,
                response_timeout=5,
                # The stalled writer cannot record a delivery.
                recording_receipts=False,
            ) as client:
                try:
                    client.initialize_and_list_tools()
                    observed.wait("protocol journal writer stalled", client=client)
                    # The stalled writer holds neither this reply nor the
                    # reception of the frames that follow it.
                    client.expect("\n[idle]", session="default")
                    client.notify("notifications/cancelled", requestId=1_000_000)
                    assert client.request("ping")["result"] == {}
                    client.stdin.close()
                finally:
                    release.release()
                transcript = client.finish()
            (recording,) = (root / ".agents/console/sessions").iterdir()
            events = recording_events(recording)
            assert [e["call_id"] for e in events if e["event"] == "tool_call"] == [1]
            assert [e["call_id"] for e in events if e["event"] == "tool_result"] == [1]
            received = [
                e["message"]
                for e in events
                if e["event"] == "protocol" and e["direction"] == "received"
            ]
            assert [e["method"] for e in received][-3:] == [
                "tools/call",
                "notifications/cancelled",
                "ping",
            ]
            assert events[-1]["event"] == "recording_ended"
            return transcript


@requires(POSIX)
def test_restart_failures_only_number_acquired_workers(binary: Path) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        worker = root / "worker"
        source = f'#!/bin/sh\nexec "{zod}"\n'
        worker.write_text(source)
        worker.chmod(0o700)
        control = root / "startup-control"
        control.write_text("ready")
        with McpClient(
            binary,
            DIRECT.serve("--worker", str(worker)),
            {**os.environ, "ZOD_STARTUP_CONTROL": str(control)},
            root,
        ) as client:
            client.initialize_and_list_tools()
            assert not client.send(r="echo ready")["isError"]
            control.write_text("fail with stderr")
            assert client.send(control="restart")["isError"]
            worker.unlink()
            assert client.send(control="restart")["isError"]
            worker.write_text(source)
            worker.chmod(0o700)
            control.write_text("ready")
            assert not client.send(control="restart")["isError"]
            assert not client.send(r="echo recovered")["isError"]
            client.finish()
        (recording,) = (root / ".agents/console/sessions").iterdir()
        events = recording_events(recording)
        assert [e["session_id"] for e in events if e["event"] == "worker_started"] == [
            1,
            2,
            3,
        ]
        assert [e["session_id"] for e in events if e["event"] == "worker_ready"] == [
            1,
            3,
        ]
        assert [e["session_id"] for e in events if e["event"] == "worker_ended"] == [
            1,
            2,
            3,
        ]
        failures = [e for e in events if e["event"] == "startup_failed"]
        assert [e["session_id"] for e in failures] == [2, None]
        assert (
            failures[0]["message"]
            == "worker sideband read failed: worker sideband closed"
        )
        assert (
            next(
                e["outcome"]
                for e in events
                if e["event"] == "worker_ended" and e["session_id"] == 2
            )
            == "exited with code 86"
        )
        assert "failed to launch worker" in failures[1]["message"]
        # Full platform diagnostics remain in existing launch-failure transcripts.
        return [
            *client.transcript[:3],
            {
                "worker sessions": [1, 2, 3],
                "readiness": [1, 3],
                "startup failures": [2, None],
            },
        ]


def first_send_without_code(binary: Path, arguments: dict) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        with McpClient(
            binary, DIRECT.serve("--worker", str(worker)), current_directory=root
        ) as client:
            client.initialize_and_list_tools()
            assert not (root / ".agents/console/sessions").exists()
            client.send(**arguments)
            transcript = client.finish()
        (recording,) = (root / ".agents/console/sessions").iterdir()
        events = recording_events(recording)
        assert [
            e["request"]["arguments"] for e in events if e["event"] == "tool_call"
        ] == [arguments]
        assert not any(e["event"] == "cell_output" for e in events)
        assert events[-1]["event"] == "recording_ended"
        return transcript


@requires(POSIX)
def test_first_poll_activates_recording(binary: Path) -> Transcript:
    return first_send_without_code(binary, {})


@requires(POSIX)
def test_first_input_activates_recording(binary: Path) -> Transcript:
    return first_send_without_code(binary, {"stdin": "unused input"})


@requires(POSIX)
def test_first_control_activates_recording(binary: Path) -> Transcript:
    return first_send_without_code(binary, {"control": "restart"})


@requires(NATIVE_FIXTURES)
@executions(DIRECT)
def test_releases_completed_stdin_requests(
    binary: Path, execution: Execution
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        workspace = Path(directory)
        with closing(RetentionProfile(workspace)) as profile:
            environment = {**os.environ, **profile.environment}
            with McpClient(
                binary, execution.serve("--worker", str(worker)), environment, workspace
            ) as client:
                client.initialize_and_list_tools()
                client.send(r="echo ready")
                retained = []
                for _ in range(2):
                    for _ in range(24):
                        client.send(stdin="x" * (1024 * 1024))
                    client.send(control="restart")
                    assert client.request("ping")["result"] == {}
                    retained.append(profile.live)
                # Permit live transport copies, but not another batch of retained input.
                assert retained[1] - retained[0] < 4 * 1024 * 1024, retained
                client.finish()
                return [
                    *client.transcript[:3],
                    {"request memory": "released across batches and restart"},
                ]


if __name__ == "__main__":
    run_this_suite(__file__)
