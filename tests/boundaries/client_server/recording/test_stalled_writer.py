#!/usr/bin/env -S uv run --script

import json
import os
import sys
import tempfile
from collections.abc import Iterator
from contextlib import closing, contextmanager
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.integrations.test_control import (
    ControlClient,
    result_text,
)
from boundaries.client_server.recording.test_process import recording_events
from support.allocations import AllocationProfile
from support.checkpoints import FifoCheckpoint, wait_for_path
from support.client import McpClient
from support.execution import DIRECT
from support.native import LOADER_VARIABLE, build_interposer
from support.previews import OMISSION, TEXT_BUDGET, assert_preview
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, POSIX, requires
from support.suites import run_this_suite

BEHIND = "recording writer stalled; retained files may be missing or incomplete"
UNPUBLISHED = "recording writer stalled; retained files are not yet published"


@contextmanager
def stalled_writer(
    binary: Path, observation: dict[str, str], *arguments: str
) -> Iterator[tuple[McpClient, Path, FifoCheckpoint, FifoCheckpoint]]:
    """Hold the first matching recording write until the release checkpoint."""
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / "startup-control").write_text("ready")
        with (
            closing(FifoCheckpoint.create(root / "observed")) as observed,
            closing(FifoCheckpoint.create(root / "release")) as release,
            McpClient(
                binary,
                DIRECT.serve(
                    "--worker",
                    str(Path(__file__).resolve().parents[3] / "fixtures/zod"),
                    *arguments,
                ),
                {
                    **os.environ,
                    LOADER_VARIABLE: str(
                        build_interposer(root, "recording_observation")
                    ),
                    **observation,
                    "ZOD_STARTUP_CONTROL": str(root / "startup-control"),
                    "ZOD_STARTUP_STARTED": str(root / "startup-started"),
                    "ZOD_STARTUP_RELEASE": str(root / "startup-release"),
                    "MCP_CONSOLE_TEST_JOURNAL_OBSERVED": str(observed.path),
                    "MCP_CONSOLE_TEST_JOURNAL_RELEASE": str(release.path),
                },
                root,
                response_timeout=15,
                # The stalled writer cannot record a delivery.
                recording_receipts=False,
            ) as client,
        ):
            client.initialize_and_list_tools()
            yield client, root, observed, release


def response_text(entry: dict) -> str:
    return "".join(
        block["text"] for block in entry["result"]["content"] if block["type"] == "text"
    )


def completed_calls(root: Path, count: int = 3) -> list[dict]:
    (recording,) = (root / ".agents/console/sessions").iterdir()
    events = recording_events(recording)
    calls = [e["call_id"] for e in events if e["event"] == "tool_call"]
    results = [e["call_id"] for e in events if e["event"] == "tool_result"]
    assert calls == results == list(range(1, count + 1)), (calls, results)
    assert events[-1]["event"] == "recording_ended"
    return events


@requires(NATIVE_FIXTURES, POSIX)
def test_stalled_result_write_does_not_hold_replies(binary: Path) -> Transcript:
    with stalled_writer(binary, {"MCP_CONSOLE_TEST_OBSERVED_EVENT": "tool_result"}) as (
        client,
        root,
        observed,
        release,
    ):
        first = client.start_send(r="echo first")
        try:
            observed.wait("tool_result journal write is stalled", client=client)
            # The journal still lacks this result. Its reply, a poll, and a new
            # cell must all complete without the recording writer.
            client.receive(first)
            assert response_text(first) == "zod: first\n", first
            client.expect("\n[idle]")
            client.expect("zod: second\n", r="echo second")
        finally:
            release.release()
        transcript = client.finish()
        completed_calls(root)
        return transcript


@requires(NATIVE_FIXTURES, POSIX)
def test_stalled_output_file_write_does_not_hold_replies(binary: Path) -> Transcript:
    # Journal lines quote the same text, so select the cell file by its path.
    with stalled_writer(
        binary, {"MCP_CONSOLE_TEST_OBSERVED_FILE": "/cell-001.txt"}
    ) as (
        client,
        root,
        observed,
        release,
    ):
        first = client.start_send(r="echo first")
        try:
            observed.wait("cell output file write is stalled", client=client)
            client.receive(first)
            assert response_text(first) == "zod: first\n", first
            client.expect("\n[idle]")
            client.expect("zod: second\n", r="echo second")
        finally:
            release.release()
        transcript = client.finish()
        events = completed_calls(root)
        (recording,) = (root / ".agents/console/sessions").iterdir()
        outputs = recording / "capture/sessions/001"
        assert (outputs / "cell-001.txt").read_text() == "zod: first\n"
        assert (outputs / "cell-002.txt").read_text() == "zod: second\n"
        assert [
            (e["path"], e["retained_bytes"], e["discarded_bytes"])
            for e in events
            if e["event"] == "cell_output"
        ] == [
            ("capture/sessions/001/cell-001.txt", 11, 0),
            ("capture/sessions/001/cell-002.txt", 12, 0),
        ]
        return transcript


@requires(NATIVE_FIXTURES, POSIX)
def test_oversized_reply_reports_a_stalled_recording_writer(binary: Path) -> Transcript:
    emitted = (
        "preview head\n"
        + "ab" * 12000
        + "\npreview tail: final diagnostic\nafter final image\n"
    )
    with stalled_writer(
        binary, {"MCP_CONSOLE_TEST_OBSERVED_FILE": "/cell-002.txt"}
    ) as (
        client,
        root,
        observed,
        release,
    ):
        client.expect("zod: ready\n", r="echo ready")
        oversized = client.start_send(r="preview tiny events")
        try:
            observed.wait("cell output file write is stalled", client=client)
            # The omission marker cites a file. Its reply waits a bounded time
            # for the writer, then names the path and says what it cannot promise.
            client.receive(oversized)
            text = response_text(oversized)
            assert_preview(text, emitted)
            (recording,) = (root / ".agents/console/sessions").iterdir()
            log = (
                f".agents/console/sessions/{recording.name}"
                "/capture/sessions/001/cell-002.txt"
            )
            (marker,) = OMISSION.finditer(text)
            assert marker[0].endswith(f"; raw log: {log}; {BEHIND}]\n"), marker[0]
            # Ordinary replies do not wait for the stalled writer at all.
            client.expect("zod: later\n", r="echo later")
        finally:
            release.release()
        transcript = client.finish()
        completed_calls(root)
        assert (recording / "capture/sessions/001/cell-002.txt").read_text() == emitted
        return [
            *transcript[:3],
            {
                "oversized reply": "bounded wait; raw log path and stalled writer named",
                "ordinary reply": "returned while the writer was stalled",
                "raw log after release": "complete",
            },
        ]


@requires(NATIVE_FIXTURES, POSIX)
def test_oversized_reply_reports_an_unpublished_recording(binary: Path) -> Transcript:
    emitted = (
        "preview head\n"
        + "ab" * 12000
        + "\npreview tail: final diagnostic\nafter final image\n"
    )
    with stalled_writer(
        binary, {"MCP_CONSOLE_TEST_OBSERVED_TEXT": '"method":"initialize"'}
    ) as (
        client,
        root,
        observed,
        release,
    ):
        oversized = client.start_send(r="preview tiny events")
        try:
            observed.wait("the recording's first write is stalled", client=client)
            # The writer has not published the recording, so its directory is
            # not known. The marker must not call queued output unavailable.
            client.receive(oversized)
            text = response_text(oversized)
            assert_preview(text, emitted)
            assert not (root / ".agents/console/sessions").exists()
            (marker,) = OMISSION.finditer(text)
            assert marker[0].endswith(f"; {UNPUBLISHED}]\n"), marker[0]
        finally:
            release.release()
        transcript = client.finish()
        completed_calls(root, 1)
        (recording,) = (root / ".agents/console/sessions").iterdir()
        assert (recording / "capture/sessions/001/cell-001.txt").read_text() == emitted
        return [
            *transcript[:2],
            {
                "oversized reply": "bounded wait; unpublished recording named",
                "raw log after release": "complete",
            },
        ]


@requires(NATIVE_FIXTURES, POSIX)
def test_phase_notice_overflow_reports_a_stalled_recording_writer(
    binary: Path,
) -> Transcript:
    notices = (
        "[worker sideband read failed: worker sideband closed]\n"
        "[worker exited with status 86]\n"
        "[worker stopped: in-memory state lost]\n"
        "[starting new worker]\n"
    )
    phase = "[phase: replacement]\n"
    terminal = "[worker starting]"
    # The reply fits the text budget without its phase notice and exceeds it
    # by one byte with it.
    emitted = "x" * (TEXT_BUDGET - len(notices + phase + terminal)) + "\n"
    with stalled_writer(
        binary, {"MCP_CONSOLE_TEST_OBSERVED_FILE": "/cell-002.txt"}
    ) as (
        client,
        root,
        observed,
        release,
    ):
        client.expect("zod: ready\n", r="echo ready")
        (root / "startup-control").write_text("block")
        failed = client.start_send(r=f"exit after output: {emitted}", timeout_ms=1_000)
        try:
            observed.wait("cell output file write is stalled", client=client)
            wait_for_path(
                root / "startup-started",
                "replacement worker awaits ready",
                client=client,
            )
            client.receive(failed)
            text = response_text(failed)
            assert text.endswith(notices + phase + terminal), text
            (recording,) = (root / ".agents/console/sessions").iterdir()
            log = (
                f".agents/console/sessions/{recording.name}"
                "/capture/sessions/001/cell-002.txt"
            )
            (marker,) = OMISSION.finditer(text)
            assert marker[0].endswith(f"; raw log: {log}; {BEHIND}]\n"), marker[0]
        finally:
            release.release()
            (root / "startup-release").touch()
        # The poll returns once the replacement worker is ready.
        client.send()
        assert "phase:" not in response_text(client.transcript[-1])
        client.expect("zod: replaced\n", r="echo replaced")
        transcript = client.finish()
        completed_calls(root, 4)
        assert (recording / "capture/sessions/001/cell-002.txt").read_text() == emitted
        return [
            *transcript[:3],
            {
                "phase-bearing reply": "bounded wait; raw log path and stalled writer named",
                "raw log after release": "complete",
            },
        ]


@requires(NATIVE_FIXTURES, POSIX)
def test_preview_subscription_follows_the_recording_writer(binary: Path) -> Transcript:
    # A Unix socket path must be short. Zod publishes its markers in TMPDIR.
    with (
        tempfile.TemporaryDirectory(dir="/tmp", prefix="console-stalled-") as scratch,
        stalled_writer(
            binary,
            {"MCP_CONSOLE_TEST_OBSERVED_FILE": "/cell-002.txt", "TMPDIR": scratch},
            "--control-socket",
            str(Path(scratch) / "control.sock"),
        ) as (client, root, observed, release),
    ):
        client.expect("zod: ready\n", r="echo ready")
        oversized = client.start_send(r="preview tiny events")
        try:
            observed.wait("cell output file write is stalled", client=client)
            # This reply's bounded wait finds the writer stalled.
            client.receive(oversized)
            assert BEHIND in response_text(oversized), oversized
            (recording,) = (root / ".agents/console/sessions").iterdir()
            log = (
                f".agents/console/sessions/{recording.name}"
                "/capture/sessions/001/cell-003.txt"
            )
            emitted = "x" * (2 * TEXT_BUDGET) + "\n"
            held = client.start_send(r=f"hold after output: {emitted}")
            wait_for_path(
                Path(scratch) / "zod-output-held",
                "pending oversized output",
                client=client,
            )
            with ControlClient(Path(scratch) / "control.sock") as observer:
                observer.request(op="subscribe")

                def marker() -> str:
                    event = json.loads(observer.reader.readline(timeout=30))
                    assert event["event"] == "preview", event
                    (marker,) = OMISSION.finditer(result_text(event))
                    return marker[0]

                assert marker().endswith(f"; raw log: {log}; {BEHIND}]\n")
                # The worker emits nothing more. The writer catching up is the
                # only change, and the subscription must replace its snapshot.
                release.release()
                assert marker().endswith(f"; raw log: {log}]\n")
            (Path(scratch) / "zod-release-evaluation").touch()
            client.receive(held)
        finally:
            release.release()
            (Path(scratch) / "zod-release-evaluation").touch()
        transcript = client.finish()
        # The subscription is a recorded call that outlives the held cell.
        assert recording_events(recording)[-1]["event"] == "recording_ended"
        assert (recording / "capture/sessions/001/cell-003.txt").read_text() == emitted
        return [
            *transcript[:3],
            {
                "subscription while the writer is stalled": "raw log path and stalled writer named",
                "subscription after the writer catches up": "replaced without new output; raw log path only",
            },
        ]


@requires(NATIVE_FIXTURES, POSIX)
def test_preview_subscription_follows_recording_publication(binary: Path) -> Transcript:
    # A Unix socket path must be short. Zod publishes its markers in TMPDIR.
    with (
        tempfile.TemporaryDirectory(dir="/tmp", prefix="console-stalled-") as scratch,
        stalled_writer(
            binary,
            {
                "MCP_CONSOLE_TEST_OBSERVED_TEXT": '"method":"initialize"',
                "TMPDIR": scratch,
            },
            "--control-socket",
            str(Path(scratch) / "control.sock"),
        ) as (client, root, observed, release),
    ):
        emitted = "x" * (2 * TEXT_BUDGET) + "\n"
        held = client.start_send(r=f"hold after output: {emitted}")
        try:
            observed.wait("the recording's first write is stalled", client=client)
            wait_for_path(
                Path(scratch) / "zod-output-held",
                "pending oversized output",
                client=client,
            )
            assert not (root / ".agents/console/sessions").exists()
            with ControlClient(Path(scratch) / "control.sock") as observer:
                observer.request(op="subscribe")

                def marker() -> str:
                    event = json.loads(observer.reader.readline(timeout=30))
                    assert event["event"] == "preview", event
                    (marker,) = OMISSION.finditer(result_text(event))
                    return marker[0]

                # No reply has waited for the writer, so it is not known to be
                # stalled. The output is queued for an unpublished recording.
                assert marker().endswith("; retained files are not yet published]\n")
                # The worker emits nothing more. Publication is the only
                # change, and the subscription must replace its snapshot.
                release.release()
                published = marker()
                (recording,) = (root / ".agents/console/sessions").iterdir()
                log = (
                    f".agents/console/sessions/{recording.name}"
                    "/capture/sessions/001/cell-001.txt"
                )
                assert published.endswith(f"; raw log: {log}]\n"), published
            (Path(scratch) / "zod-release-evaluation").touch()
            client.receive(held)
        finally:
            release.release()
            (Path(scratch) / "zod-release-evaluation").touch()
        transcript = client.finish()
        assert recording_events(recording)[-1]["event"] == "recording_ended"
        assert (recording / "capture/sessions/001/cell-001.txt").read_text() == emitted
        return [
            *transcript[:2],
            {
                "subscription before the recording is published": "retained files named as not yet published",
                "subscription after publication": "replaced without new output; raw log path",
            },
        ]


@requires(NATIVE_FIXTURES, POSIX)
def test_unretained_omission_does_not_wait_for_the_recording_writer(
    binary: Path,
) -> Transcript:
    # A Unix socket path must be short. Zod publishes its markers in TMPDIR.
    with (
        tempfile.TemporaryDirectory(dir="/tmp", prefix="console-stalled-") as scratch,
        stalled_writer(
            binary,
            {"MCP_CONSOLE_TEST_OBSERVED_FILE": "/cell-002.txt", "TMPDIR": scratch},
            "--control-socket",
            str(Path(scratch) / "control.sock"),
        ) as (client, root, observed, release),
    ):
        client.expect("zod: ready\n", r="echo ready")
        stalled = client.start_send(r="echo stalled")
        try:
            observed.wait("cell output file write is stalled", client=client)
            client.receive(stalled)
            (recording,) = (root / ".agents/console/sessions").iterdir()
            # The next cell's output file cannot be created, so its omitted
            # text is retained nowhere and its marker names no file.
            (recording / "capture/sessions/001/cell-003.txt").mkdir()
            client.send(r="preview tiny events")
            (marker,) = OMISSION.finditer(response_text(client.transcript[-1]))
            assert marker[0].endswith("; no retained log; omitted text unavailable]\n")
            emitted = "x" * (2 * TEXT_BUDGET) + "\n"
            held = client.start_send(r=f"hold after output: {emitted}")
            wait_for_path(
                Path(scratch) / "zod-output-held",
                "pending oversized output",
                client=client,
            )
            # A preview reports a stall only after a reply's wait for the
            # writer has found one. The reply above had no file to wait for,
            # so this preview names its raw log without that warning.
            log = (
                f".agents/console/sessions/{recording.name}"
                "/capture/sessions/001/cell-004.txt"
            )
            with ControlClient(Path(scratch) / "control.sock") as control:
                preview = control.request(op="preview")
            (marker,) = OMISSION.finditer(result_text(preview))
            assert marker[0].endswith(f"; raw log: {log}]\n"), marker[0]
            release.release()
            (Path(scratch) / "zod-release-evaluation").touch()
            client.receive(held)
        finally:
            release.release()
            (Path(scratch) / "zod-release-evaluation").touch()
        transcript = client.finish()
        # The preview is a recorded call that completes before the held cell.
        assert recording_events(recording)[-1]["event"] == "recording_ended"
        assert (recording / "capture/sessions/001/cell-004.txt").read_text() == emitted
        return [
            *transcript[:3],
            {
                "reply with unretained omitted text": "did not wait for the stalled writer",
                "later preview": "raw log path without a stalled-writer warning",
            },
        ]


@requires(NATIVE_FIXTURES, POSIX)
def test_request_burst_waits_for_a_slow_recording_writer(binary: Path) -> Transcript:
    # A Unix socket path must be short.
    with (
        tempfile.TemporaryDirectory(dir="/tmp", prefix="console-slow-") as scratch,
        stalled_writer(
            binary,
            {
                "MCP_CONSOLE_TEST_OBSERVED_EVENT": "tool_call",
                # Long enough for the whole burst to arrive behind this write,
                # and well inside the time a writer may go without progress.
                "MCP_CONSOLE_TEST_WRITE_DELAY_MS": "2000",
            },
            "--control-socket",
            str(Path(scratch) / "control.sock"),
        ) as (client, root, _, _),
    ):
        # Each request queues at least its call and its result, so the burst
        # exceeds the 1,024 operations the recording queue retains. A writer
        # that is slow but still writing must hold requests back, not lose
        # the recording.
        burst = 1100
        with ControlClient(Path(scratch) / "control.sock") as control:
            for _ in range(burst):
                response = control.request(op="status")
                assert response["result"]["state"] == "idle", response
        transcript = client.finish()
        completed_calls(root, burst)
        return transcript


@requires(NATIVE_FIXTURES, POSIX)
def test_pipelined_requests_wait_for_a_slow_recording_writer(
    binary: Path,
) -> Transcript:
    with stalled_writer(
        binary,
        {
            "MCP_CONSOLE_TEST_OBSERVED_TEXT": '"method":"ping"',
            # As above: the first ping's journal write is slow, not stalled.
            "MCP_CONSOLE_TEST_WRITE_DELAY_MS": "2000",
        },
    ) as (client, root, _, _):
        # Pings alone do not publish a recording.
        client.expect("zod: ready\n", r="echo ready")
        # Each ping queues at least its receipt, so the burst exceeds the
        # 1,024 operations the recording queue retains.
        pings = client.send_messages(
            [
                {"jsonrpc": "2.0", "id": f"ping-{number}", "method": "ping"}
                for number in range(1100)
            ]
        )
        client.receive_many(pings)
        assert all(ping["result"] == {} for ping in pings), pings
        transcript = client.finish()
        received = [
            event["message"]["id"]
            for event in completed_calls(root, 1)
            if event["event"] == "protocol"
            and event["direction"] == "received"
            and event["message"].get("method") == "ping"
        ]
        assert received == [ping["id"] for ping in pings], received
        return [
            *transcript[:4],
            {"pipelined pings behind a slow writer": "all answered and recorded"},
        ]


@requires(NATIVE_FIXTURES, POSIX)
def test_stopped_recording_leaves_file_closes_to_the_writer(binary: Path) -> Transcript:
    with (
        tempfile.TemporaryDirectory() as scratch,
        closing(FifoCheckpoint.create(Path(scratch) / "reached")) as close_reached,
        closing(FifoCheckpoint.create(Path(scratch) / "release")) as close_release,
        stalled_writer(
            binary,
            {
                "MCP_CONSOLE_TEST_OBSERVED_FILE": "/cell-001.txt",
                "MCP_CONSOLE_TEST_HELD_CLOSE": "/cell-002.txt",
                "MCP_CONSOLE_TEST_CLOSE_REACHED": str(close_reached.path),
                "MCP_CONSOLE_TEST_CLOSE_RELEASE": str(close_release.path),
            },
        ) as (client, root, observed, release),
    ):
        first = client.start_send(r="echo first")
        try:
            try:
                observed.wait("cell output file write is stalled", client=client)
                client.receive(first)
                # The second cell's file is open. Its write and its close wait
                # behind the stalled write.
                client.expect("zod: second\n", r="echo second")
                # Requests alone pass the intake bound. After 5 seconds without
                # writer progress, recording stops and discards what is queued.
                # A discarded close must not run on the thread that stopped
                # recording: this one never returns, and every later request
                # would wait behind it.
                for _ in range(100):
                    assert client.request("ping")["result"] == {}
                client.expect("zod: third\n", r="echo third")
            finally:
                release.release()
            close_reached.wait("the writer closes the discarded file", client=client)
            # The writer's close holds no request either.
            assert client.request("ping")["result"] == {}
        finally:
            close_release.release()
        client.await_recording_disabled()
        transcript, diagnostics = client.finish_with_standard_error()
        assert diagnostics == (
            "mcp-console: transcript recording disabled: "
            "recording writer made no progress for 5 seconds\n"
        ), diagnostics
        (recording,) = (root / ".agents/console/sessions").iterdir()
        # Recording stopped before the third cell, which has no file.
        assert sorted(
            path.name for path in (recording / "capture/sessions/001").iterdir()
        ) == ["cell-001.txt", "cell-002.txt"]
        return [
            *transcript[:5],
            {"requests after recording stops": "answered while a file close is held"},
        ]


@requires(NATIVE_FIXTURES, POSIX)
def test_stalled_writer_does_not_grow_queued_output_buffers(binary: Path) -> Transcript:
    emitted = "preview head\n" + "ab" * 100000 + "\npreview tail: final diagnostic\n"
    with (
        tempfile.TemporaryDirectory() as scratch,
        closing(AllocationProfile(Path(scratch))) as profile,
        stalled_writer(
            binary,
            {
                **profile.environment,
                # Both interposers wrap `write`; each passes the call on.
                LOADER_VARIABLE: os.pathsep.join(
                    [
                        profile.environment[LOADER_VARIABLE],
                        str(build_interposer(Path(scratch), "recording_observation")),
                    ]
                ),
                "MCP_CONSOLE_TEST_OBSERVED_FILE": "/cell-002.txt",
            },
        ) as (client, root, observed, release),
    ):
        client.expect("zod: ready\n", r="echo ready")
        profile.start()
        flood = client.start_send(r="preview same producer")
        try:
            observed.wait("cell output file write is stalled", client=client)
            # Every later two-byte write of this cell queues behind the held
            # one. The queue joins them into ranges; no range, and nothing
            # else, may need a buffer above the bound that tiny events keep
            # while the writer keeps up.
            client.receive(flood)
            assert client.request("ping")["result"] == {}
            _, largest = profile.stop()
        finally:
            release.release()
        assert largest <= 128 * 1024, largest
        text = response_text(flood)
        assert_preview(text, emitted)
        (marker,) = OMISSION.finditer(text)
        assert marker[0].endswith(f"; {BEHIND}]\n"), marker[0]
        transcript = client.finish()
        completed_calls(root, 2)
        (recording,) = (root / ".agents/console/sessions").iterdir()
        assert (recording / "capture/sessions/001/cell-002.txt").read_text() == emitted
        return [
            *transcript[:3],
            {
                "output queued behind the stalled writer": "bounded buffers",
                "raw log after release": "complete",
            },
        ]


if __name__ == "__main__":
    run_this_suite(__file__)
