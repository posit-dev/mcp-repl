#!/usr/bin/env -S uv run --script

import os
import re
import sys
import tempfile
from collections.abc import Callable, Iterator
from contextlib import closing, contextmanager
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server._harness import expose_idle_sideband_output
from support.progress import elapsed_progress, phase_progress, without_elapsed
from support.allocations import AllocationProfile
from support.assertions import last_tool_text
from support.checkpoints import (
    FifoCheckpoint,
    release_fixture_checkpoint,
    wait_for_path,
    wait_for_worker_file,
)
from support.client import McpClient
from support.execution import DIRECT
from support.previews import (
    assert_preview,
    compact_previews,
    cell_text,
    normalize_preview_paths,
    live_recording_events,
    session_directory,
    TEXT_BUDGET,
)
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, requires
from support.suites import run_this_suite

PENDING_TEXT_BUDGET = 8 * 1024 * 1024


@requires(NATIVE_FIXTURES)
def test_cancelled_controls_recover_output(binary: Path) -> Transcript:
    return cancelled_control_recovery(binary, count=4)


@requires(NATIVE_FIXTURES)
def test_cancelled_control_recovery_keeps_bounded_allocations(
    binary: Path,
) -> Transcript:
    return cancelled_control_recovery(binary, count=1024, allocation_limit=256 * 1024)


def cancelled_control_recovery(
    binary: Path, *, count: int, allocation_limit: int | None = None
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with (
            closing(FifoCheckpoint.create(root / "result-reached")) as reached,
            closing(FifoCheckpoint.create(root / "result-release")) as release,
            closing(AllocationProfile(root)) as profile,
            McpClient(
                binary,
                DIRECT.serve("--worker", str(worker)),
                {
                    **os.environ,
                    **profile.environment,
                    "MCP_CONSOLE_TEST_RESULT_REACHED": str(reached.path),
                    "MCP_CONSOLE_TEST_RESULT_RELEASE": str(release.path),
                },
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.send(r="echo ready")
            profile.start()
            profile.pause_results(True)
            cancellation_start = len(client.transcript)
            try:
                for _ in range(count):
                    pending = client.start_send(control="interrupt")
                    cancel_result(client, pending, reached, release)
            finally:
                profile.pause_results(False)
                release.release()
            compact_cancelled_exchanges(
                client,
                cancellation_start,
                count=count,
                reply=lambda index: "\n[idle]" * (index + 1),
            )
            result = client.send(control="interrupt")
            client.request("ping")
            _, largest = profile.stop()
            # Allow the retained control notices and temporary vector growth,
            # but not an additional history of invisible source receipts.
            if allocation_limit is not None:
                assert largest <= allocation_limit, largest
            assert not result["isError"], result
            text = result["content"][0]["text"]
            assert len(text.encode()) <= 8192
            assert text == "\n[idle]" * (count + 1)
            client.send(session="default")
            assert last_tool_text(client) == "\n[idle]"
            compact_previews(
                client,
                "\n[idle]",
                """
[worker stopped: in-memory state lost]
[starting new worker]
[done]""",
                "x",
            )
            received = [
                event["message"]["method"]
                for event in live_recording_events(session_directory(client))
                if event["event"] == "protocol" and event["direction"] == "received"
            ]
            # Protocol capture must retain the cancellation and ping that the
            # server received while each reply's write was held.
            assert (
                received[4 : 4 + 3 * count]
                == [
                    "tools/call",
                    "notifications/cancelled",
                    "ping",
                ]
                * count
            )
            return client.finish()


@requires(NATIVE_FIXTURES)
def test_recovered_recorded_cells_keep_bounded_source_markers(
    binary: Path,
) -> Transcript:
    return recovered_recorded_cells(binary, count=32, silent=False)


@requires(NATIVE_FIXTURES)
def test_recovered_silent_cells_discard_file_receipts(binary: Path) -> Transcript:
    return recovered_recorded_cells(binary, count=4, silent=True)


@requires(NATIVE_FIXTURES)
def test_recovered_silent_cells_keep_bounded_allocations(binary: Path) -> Transcript:
    return recovered_recorded_cells(
        binary, count=512, silent=True, allocation_limit=64 * 1024
    )


def recovered_recorded_cells(
    binary: Path, *, count: int, silent: bool, allocation_limit: int | None = None
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with (
            closing(FifoCheckpoint.create(root / "result-reached")) as reached,
            closing(FifoCheckpoint.create(root / "result-release")) as release,
            closing(AllocationProfile(root)) as profile,
            McpClient(
                binary,
                DIRECT.serve("--worker", str(worker)),
                {
                    **os.environ,
                    **profile.environment,
                    "MCP_CONSOLE_TEST_RESULT_REACHED": str(reached.path),
                    "MCP_CONSOLE_TEST_RESULT_RELEASE": str(release.path),
                },
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.send(r="echo ready")
            profile.start()
            profile.pause_results(True)
            cancellation_start = len(client.transcript)
            try:
                for index in range(count):
                    # Keep one worker for silent-cell stress. The recorded-text
                    # case also exercises recovery across worker restarts.
                    pending = client.start_send(
                        control="interrupt" if silent else "restart",
                        r="complete silently"
                        if silent
                        else f"preview recovery cell {index}",
                    )
                    cancel_result(client, pending, reached, release)
                    if not silent:
                        # Each cancelled reply replays the bounded output that
                        # earlier cancelled replies did not deliver.
                        replayed = pending["result"]["content"][0]["text"]
                        assert len(replayed.encode()) <= 8192
                        assert "cell 0 head\n" in replayed, replayed[:200]
                        assert replayed.endswith(f"cell {index} tail\n[done]")
                        assert replayed.count("[output omitted:") == 1
            finally:
                profile.pause_results(False)
                release.release()
            compact_cancelled_exchanges(
                client,
                cancellation_start,
                count=count,
                reply=(lambda index: "\n".join(["[done]"] * (index + 1)))
                if silent
                else None,
                source_template=None if silent else "preview recovery cell {index}",
            )
            result = client.send(control="interrupt")
            client.request("ping")
            _, largest = profile.stop()
            if allocation_limit is not None:
                # Control history is already bounded independently of cell text. Repeated
                # empty files must not add another unbounded receipt history.
                assert largest <= allocation_limit, largest
            assert not result["isError"], result
            text = result["content"][0]["text"]
            assert len(text.encode()) <= 8192
            assert text.endswith("[done]"), repr(text[-200:])
            events = live_recording_events(session_directory(client))
            summaries = {
                event["call_id"]: event
                for event in events
                if event["event"] == "cell_output"
            }
            assert len(summaries) == count + 1, summaries.keys()
            if silent:
                assert text == "\n".join(["[done]"] * count)
                assert "output preview" not in text
            else:
                assert "cell 0 head\n" in text and f"cell {count - 1} tail\n" in text
                assert text.count("[output omitted:") == 1, text
                assert (
                    text.count(
                        f".agents/console/sessions/{session_directory(client).name}/capture/sessions/"
                    )
                    == 1
                )
                assert "retained logs:" in text
                omitted = sum(
                    map(int, re.findall(r"output omitted: (\d+) UTF-8 bytes", text))
                )
                assert (
                    sum(s["inline_omitted_bytes"] for s in summaries.values())
                    == omitted
                )
            for index in range(count):
                emitted = (
                    ""
                    if silent
                    else f"cell {index} head\n" + "x" * 32768 + f"\ncell {index} tail\n"
                )
                assert cell_text(client, index + 2) == emitted
                summary = summaries[index + 2]
                assert summary["retained_bytes"] == len(emitted.encode())
                assert summary["discarded_bytes"] == 0
                if silent:
                    assert summary["inline_omitted_bytes"] == 0
                elif 0 < index < count - 1:
                    assert summary["inline_omitted_bytes"] == len(emitted.encode())
            client.send(session="default")
            assert last_tool_text(client) == "\n[idle]"
            normalize_preview_paths(client)
            compact_previews(
                client,
                "\n[idle]",
                """
[worker stopped: in-memory state lost]
[starting new worker]
[done]""",
                "x",
                "\n[done]",
            )
            return client.finish()


def compact_cancelled_exchanges(
    client: McpClient,
    start: int,
    *,
    count: int,
    reply: Callable[[int], str] | None,
    source_template: str | None = None,
) -> None:
    # Verify every repeated setup exchange before recording one representative.
    # The full response is checked before any lossless repetition notation.
    # `reply` gives the text of each cancelled reply; a caller passing None has
    # already checked that text.
    entries = client.transcript[start:]
    assert len(entries) == 3 * count
    for index, (pending, cancelled, ping) in enumerate(
        zip(entries[::3], entries[1::3], entries[2::3], strict=True)
    ):
        expected = dict(entries[0]["send"])
        if source_template is not None:
            expected["r"] = source_template.format(index=index)
        replayed = pending["result"]["content"][0]["text"]
        assert pending == {
            "id": pending["id"],
            "send": expected,
            "result": {
                "content": [{"type": "text", "text": replayed}],
                "isError": False,
            },
        }
        if reply is not None:
            assert replayed == reply(index), (index, replayed)
        assert cancelled == {
            "input": {
                "method": "notifications/cancelled",
                "params": {"requestId": pending["id"]},
            }
        }
        assert ping == {"id": ping["id"], "input": {"method": "ping"}, "result": {}}
    entries[0]["transcript_normalization"] = {
        "target": "repeated cancellation setup exchanges",
        "repeated_exchange_count": count,
        "exchange_entry_count": 3,
        "request_ids": "distinct in each exchange",
        "result": "each cancelled reply also replays the output recovered so far",
    }
    if source_template is not None:
        entries[0]["transcript_normalization"]["send.r"] = {
            "template": source_template,
            "index_start": 0,
            "index_count": count,
        }
    client.transcript[start:] = entries[:3]


@contextmanager
def recovery_client(
    binary: Path,
    *,
    recording_directory: Path | None = None,
    block_replacement: bool = False,
    file_limit: int | None = None,
) -> Iterator[tuple[McpClient, AllocationProfile, FifoCheckpoint, FifoCheckpoint]]:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        if block_replacement:
            (root / "startup-control").write_text("ready")
        with (
            closing(FifoCheckpoint.create(root / "result-reached")) as reached,
            closing(FifoCheckpoint.create(root / "result-release")) as release,
            closing(AllocationProfile(root)) as profile,
            McpClient(
                binary,
                DIRECT.serve("--worker", str(worker)),
                {
                    **os.environ,
                    **profile.environment,
                    "MCP_CONSOLE_TEST_RESULT_REACHED": str(reached.path),
                    "MCP_CONSOLE_TEST_RESULT_RELEASE": str(release.path),
                    "MCP_CONSOLE_TEST_PREVIEW_DIRECTORY": str(root),
                    "TMPDIR": str(root),
                    **(
                        {"MCP_CONSOLE_TEST_FILE_LIMIT": str(file_limit)}
                        if file_limit is not None
                        else {}
                    ),
                    **(
                        {
                            "ZOD_STARTUP_CONTROL": str(root / "startup-control"),
                            "ZOD_STARTUP_STARTED": str(root / "startup-started"),
                            "ZOD_STARTUP_RELEASE": str(root / "startup-release"),
                        }
                        if block_replacement
                        else {}
                    ),
                    **(
                        {"MCP_CONSOLE_HOME": str(recording_directory)}
                        if recording_directory is not None
                        else {}
                    ),
                },
                record_in_project=recording_directory is None,
                use_home_configuration=recording_directory is not None,
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.send(r="echo ready")
            profile.pause_results(True)
            try:
                yield client, profile, reached, release
            finally:
                profile.pause_results(False)
                release.release()
                if block_replacement:
                    (root / "startup-release").touch()


def cancel_result(
    client: McpClient,
    pending: dict,
    reached: FifoCheckpoint,
    release: FifoCheckpoint,
) -> None:
    """Cancel a call once the write of its assembled reply is held."""
    reached.wait("reply write starts with its result assembled", client=client)
    cancel_held_result(client, pending, release)


def cancel_held_result(
    client: McpClient, pending: dict, release: FifoCheckpoint
) -> None:
    """Cancel a call while the write of its assembled reply is held.

    The held write blocks every later reply, so the recorded receipt of a
    following ping proves that the server applied the cancellation first. The
    client then receives the reply it no longer wants.
    """
    client.notify("notifications/cancelled", requestId=pending["id"])
    ping = client.start_request("ping")
    client.await_received(ping)
    release.release()
    client.receive(pending)
    client.receive(ping)
    assert ping["result"] == {}


@requires(NATIVE_FIXTURES)
def test_cancelled_aggregate_poll_returns_each_sessions_output(
    binary: Path,
) -> Transcript:
    with recovery_client(binary) as (client, profile, reached, release):
        for session in ("alpha", "beta"):
            pending = client.start_send(r=f"echo {session} output", session=session)
            cancel_result(client, pending, reached, release)

        # Each completed cell still owns its undelivered response. Cancelling
        # their shared poll must return both receipts to their original owners.
        pending = client.start_send(timeout_ms=0)
        cancel_result(client, pending, reached, release)
        profile.pause_results(False)
        for session in ("alpha", "beta"):
            result = client.send(session=session)
            assert result == {
                "content": [{"type": "text", "text": f"zod: {session} output\n"}],
                "isError": False,
            }, result
            client.send(session=session)
            assert last_tool_text(client) == "\n[idle]"
        capture = live_recording_events(session_directory(client))
        (aggregate_call,) = [
            event
            for event in capture
            if event["event"] == "tool_call" and event["request_id"] == pending["id"]
        ]
        (aggregate_result,) = [
            event
            for event in capture
            if event["event"] == "tool_result"
            and event["call_id"] == aggregate_call["call_id"]
        ]
        content = aggregate_result["result"]["content"]
        assert content == [
            {
                "type": "text",
                "text": (
                    '[session: "alpha"; status: idle]\nzod: alpha output\n'
                    '[session: "beta"; status: idle]\nzod: beta output\n'
                    '[session: "default"; status: idle]\n\n[idle]'
                ),
            },
        ], content
        workers = {
            event.get("session", "default"): event["session_id"]
            for event in capture
            if event["event"] == "worker_started"
        }
        for session in ("alpha", "beta"):
            (part,) = [
                part
                for part in aggregate_result["parts"]
                if part["owner"] is not None
                and part["owner"]["session_id"] == workers[session]
            ]
            assert part["block"] == 0, part
            assert content[0]["text"][part["start"] : part["end"]] == (
                f'[session: "{session}"; status: idle]\nzod: {session} output\n'
            ), (part, content)
        return client.finish()


@requires(NATIVE_FIXTURES)
def test_empty_poll_retains_stopped_session_output(
    binary: Path,
) -> Transcript:
    with recovery_client(binary) as (client, profile, reached, release):
        pending = client.start_send(r="echo alpha output", session="alpha")
        cancel_result(client, pending, reached, release)
        pending = client.start_send(control="restart", session="alpha")
        cancel_result(client, pending, reached, release)

        # Cancelling the aggregate poll must restore the stopped session's
        # retained output along with the default session's idle response.
        pending = client.start_send(timeout_ms=0)
        cancel_result(client, pending, reached, release)
        profile.pause_results(False)
        result = client.send(timeout_ms=0)
        assert result == {
            "content": [
                {
                    "type": "text",
                    "text": (
                        '[session: "alpha"; status: stopped]\n'
                        "zod: alpha output\n"
                        "[worker stopped: in-memory state lost]\n"
                        "[stopped]\n[stopped]\n[stopped]\n"
                        '[session: "default"; status: idle]\n\n[idle]\n[idle]'
                    ),
                }
            ],
            "isError": False,
        }, result
        # Neither a targeted nor an aggregate cancelled poll of the now-empty
        # stopped session may turn its status notice into pending output.
        profile.pause_results(True)
        for arguments in ({"session": "alpha"}, {}):
            pending = client.start_send(timeout_ms=0, **arguments)
            cancel_result(client, pending, reached, release)
        profile.pause_results(False)
        result = client.send(timeout_ms=0)
        assert result == {
            "content": [{"type": "text", "text": "\n[idle]\n[idle]"}],
            "isError": False,
        }, result
        assert client.send(timeout_ms=0) == {
            "content": [{"type": "text", "text": "\n[idle]"}],
            "isError": False,
        }
        return client.finish()


@requires(NATIVE_FIXTURES)
def test_cancelled_idle_polls_retain_output_until_delivery(binary: Path) -> Transcript:
    with recovery_client(binary) as (client, profile, reached, release):
        profile.pause_results(False)
        # Emit output only after the cell's completion has been delivered, and
        # wait for the sideband receipt before cancelling either kind of poll.
        expose_idle_sideband_output(client, reached.path.parent)
        profile.pause_results(True)
        for arguments in ({"session": "default"}, {}):
            pending = client.start_send(timeout_ms=0, **arguments)
            cancel_result(client, pending, reached, release)
        profile.pause_results(False)
        result = client.send(timeout_ms=0)
        assert result == {
            "content": [
                {
                    "type": "text",
                    "text": "zod background sideband\n\n[idle]\n[idle]\n[idle]",
                }
            ],
            "isError": False,
        }, result
        assert client.send(timeout_ms=0) == {
            "content": [{"type": "text", "text": "\n[idle]"}],
            "isError": False,
        }
        return client.finish()


@requires(NATIVE_FIXTURES)
def test_cancelled_aggregate_poll_keeps_a_file_failure_notice(
    binary: Path,
) -> Transcript:
    file_limit = 3 * PENDING_TEXT_BUDGET + 3
    with recovery_client(binary, file_limit=file_limit) as (
        client,
        profile,
        reached,
        release,
    ):
        pending = client.start_send(r="echo beta output", session="beta")
        cancel_result(client, pending, reached, release)
        profile.pause_results(False)
        client.send(r="overflow cell output file", session="alpha", timeout_ms=0)
        assert (
            without_elapsed(last_tool_text(client))
            == "\n[running; poll with an empty send]"
        )
        gate = wait_for_worker_file(
            reached.path.parent, "zod-release-spooled-output", client
        )
        with closing(
            FifoCheckpoint.attach(gate.with_name("zod-spooled-output-processed"))
        ) as processed:
            # The first batch outgrows the file limit before any reply waits
            # for the writer, so the aggregate poll is the first to report it.
            release_fixture_checkpoint(gate, client=client)
            processed.wait(timeout=client.response_timeout)
            profile.pause_results(True)
            pending = client.start_send(timeout_ms=0)
            cancel_result(client, pending, reached, release)
            profile.pause_results(False)
            failure = f"stopped after {file_limit} retained bytes"
            (cancelled,) = pending["result"]["content"]
            assert failure in cancelled["text"], cancelled["text"]
            assert "zod: beta output\n" in cancelled["text"], cancelled["text"]

            # Each session's recovered output still reports the failed file.
            client.send(timeout_ms=0)
            replayed = last_tool_text(client)
            assert failure in replayed, replayed
            assert "zod: beta output\n" in replayed, replayed
            assert replayed.count("[cell output file ") == 1, replayed

            release_fixture_checkpoint(gate, client=client)
            processed.wait(timeout=client.response_timeout)
        client.send(session="alpha")
        assert "later raw text unavailable" in last_tool_text(client)
        client.send(session="alpha")
        assert last_tool_text(client) == "\n[idle]"
    return [
        {
            "cancelled_aggregate_reports_file_failure": True,
            "recovered_aggregate_reports_file_failure_once": True,
        }
    ]


@requires(NATIVE_FIXTURES)
def test_aggregate_poll_keeps_one_text_budget(binary: Path) -> Transcript:
    with recovery_client(binary) as (client, profile, reached, release):
        for number, session in enumerate(("alpha", "beta")):
            pending = client.start_send(
                r=f"preview recovery cell {number}", session=session
            )
            cancel_result(client, pending, reached, release)
        profile.pause_results(False)
        result = client.send(timeout_ms=0)
        assert not result["isError"], result
        output = "".join(part["text"] for part in result["content"])
        assert len(output.encode()) <= TEXT_BUDGET, len(output.encode())
        assert "cell 0 head\n" in output and "\ncell 1 tail\n" in output, output
        capture = live_recording_events(session_directory(client))
        calls = {
            event["request"]["arguments"].get("r"): event["call_id"]
            for event in capture
            if event["event"] == "tool_call"
        }
        summaries = {
            event["call_id"]: event
            for event in capture
            if event["event"] == "cell_output"
        }
        owners = set()
        for number in range(2):
            summary = summaries[calls[f"preview recovery cell {number}"]]
            raw = f"cell {number} head\n" + "x" * 32768 + f"\ncell {number} tail\n"
            assert (session_directory(client) / summary["path"]).read_text() == raw
            assert summary["retained_bytes"] == len(raw.encode()), summary
            assert summary["discarded_bytes"] == 0, summary
            owners.add(summary["owner"]["session_id"])
        assert len(owners) == 2, summaries
        normalize_preview_paths(client)
        compact_previews(client, "x")
        return client.finish()


@requires(NATIVE_FIXTURES)
def test_cancelled_active_polls_replay_before_later_output(binary: Path) -> Transcript:
    with recovery_client(binary) as (client, profile, reached, release):
        root = reached.path.parent
        with (
            closing(FifoCheckpoint.create(root / "preview-release")) as worker_release,
            closing(FifoCheckpoint.create(root / "preview-processed")) as processed,
        ):
            try:
                profile.pause_results(False)
                client.send(r="preview recovery intervals", timeout_ms=0)
                assert (
                    without_elapsed(last_tool_text(client))
                    == "\n[running; poll with an empty send]"
                )
                profile.pause_results(True)
                emitted = []
                for index in range(3):
                    worker_release.release()
                    processed.wait("active cell interval reached the output tape")
                    emitted.append(
                        f"interval {index} head\n"
                        + "x" * 32768
                        + f"\ninterval {index} tail\n"
                    )
                    if index < 2:
                        pending = client.start_send(session="default", timeout_ms=0)
                        cancel_result(client, pending, reached, release)
                profile.pause_results(False)
                # The interrupt grace advances cell age before recovery. Replay
                # must still match the original journaled progress text exactly.
                result = client.send(control="interrupt", timeout_ms=0)
                assert not result["isError"], result
                recorded = next(
                    event["result"]
                    for event in live_recording_events(session_directory(client))
                    if event["event"] == "tool_result" and event["call_id"] == 3
                )
                assert result["content"] == recorded["content"]
                assert result["isError"] == recorded["isError"]
                text = result["content"][0]["text"]
                assert elapsed_progress(text)[1] is False
                state = "\n[running; poll with an empty send]"
                assert text.endswith(state)
                assert_preview(without_elapsed(text).removesuffix(state), emitted[0])
                assert "capture/events.jsonl" not in text
                assert "capture/sessions/001/cell-002.txt" in text
                assert cell_text(client, 2) == "".join(emitted)
                client.send(session="default", timeout_ms=0)
                later = last_tool_text(client)
                assert elapsed_progress(later)[1] is False
                assert later.endswith(state)
                assert_preview(
                    without_elapsed(later).removesuffix(state), "".join(emitted[1:])
                )
                assert "capture/sessions/001/cell-002.txt" in later
                worker_release.release()
                client.send(session="default")
                assert last_tool_text(client) == "[done]"
                client.send(session="default")
                assert last_tool_text(client) == "\n[idle]"
                normalize_preview_paths(client)
                compact_previews(
                    client,
                    "\n[idle]",
                    """
[worker stopped: in-memory state lost]
[starting new worker]
[done]""",
                    "x",
                )
                return client.finish()
            finally:
                for _ in range(4):
                    worker_release.release()


@requires(NATIVE_FIXTURES)
def test_image_recovery_moves_the_retained_preview(binary: Path) -> Transcript:
    with recovery_client(binary) as (client, profile, reached, release):
        pending = client.start_send(r="preview allocation image")
        cancel_result(client, pending, reached, release)
        [cancelled] = pending["result"]["content"]
        assert cancelled == {
            "type": "image",
            "data": "A" * (8 * 1024 * 1024),
            "mimeType": "image/png",
        }
        cancelled["data"] = "<image byte-identical to 6 MiB of zero bytes>"
        # Stop where the reply's write starts: image ingestion, persistence,
        # and MCP serialization have finished.
        profile.start()
        recovered = client.start_send(session="default")
        reached.wait("recovered image reply starts its write")
        allocated, _ = profile.stop()
        release.release()
        client.receive(recovered)
        result = recovered["result"]
        assert result == {
            "content": [
                {
                    "type": "image",
                    "data": "A" * (8 * 1024 * 1024),
                    "mimeType": "image/png",
                }
            ],
            "isError": False,
        }
        session = session_directory(client)
        events = live_recording_events(session)
        artifacts = [event for event in events if event["event"] == "image_retained"]
        assert len(artifacts) == 1, artifacts
        artifact = artifacts[0]
        assert artifact["mime_type"] == "image/png"
        assert artifact["bytes"] == 6 * 1024 * 1024
        assert (session / artifact["path"]).read_bytes() == bytes(6 * 1024 * 1024)
        recorded = [
            event["result"]["content"]
            for event in events
            if event["event"] == "tool_result" and event["call_id"] in (2, 3)
        ]
        assert (
            recorded
            == [
                [
                    {
                        "type": "image",
                        "artifactId": artifact["artifact_id"],
                        "path": artifact["path"],
                        "mimeType": "image/png",
                    }
                ]
            ]
            * 2
        ), recorded
        # MCP requires one encoded copy and protocol capture takes a second;
        # retaining recovery must move the existing preview. The frame reuses
        # the buffer of the cancelled reply. Allow small response, journal, and
        # transport overhead.
        assert allocated < 17 * 1024 * 1024, allocated
        result["content"][0]["data"] = "<image byte-identical to 6 MiB of zero bytes>"
        profile.pause_results(False)
        client.send(session="default")
        assert last_tool_text(client) == "\n[idle]"
        return client.finish()


@requires(NATIVE_FIXTURES)
def test_phase_projection_copies_retained_image_once(binary: Path) -> Transcript:
    with (
        recovery_client(binary, block_replacement=True) as (
            client,
            profile,
            reached,
            release,
        ),
        closing(FifoCheckpoint.create(reached.path.parent / "image-release")) as emit,
    ):
        root = reached.path.parent
        profile.pause_results(False)
        client.send(r="preview allocation image then fail", timeout_ms=0)
        assert without_elapsed(last_tool_text(client)) == (
            "\n[running; poll with an empty send]"
        )
        emit.release()
        wait_for_path(
            root / "startup-started",
            "failed worker replacement awaits ready",
            client=client,
        )
        profile.pause_results(True)
        profile.start()
        pending = client.start_send(session="default", timeout_ms=0)
        reached.wait("phase-bearing image reply starts its write")
        allocated, _ = profile.stop()
        profile.pause_results(False)
        release.release()
        client.receive(pending)
        image, progress = pending["result"]["content"]
        assert image == {
            "type": "image",
            "data": "A" * (8 * 1024 * 1024),
            "mimeType": "image/png",
        }
        assert phase_progress(progress["text"]) == "replacement"
        assert progress["text"].endswith("\n[worker starting]")
        # MCP requires one encoded copy and protocol capture takes a second.
        # This first large reply also allocates its frame (8 MiB, then doubled)
        # and a 2 MiB stdout chunk. The phase overlay must not add another copy.
        assert allocated < 45 * 1024 * 1024, allocated
        image["data"] = "<image byte-identical to 6 MiB of zero bytes>"
        (root / "startup-release").touch()
        client.send(session="default")
        assert "phase:" not in last_tool_text(client)
        client.send(r="echo after replacement")
        assert last_tool_text(client) == "zod: after replacement\n"
        return client.finish()


@requires(NATIVE_FIXTURES)
def test_composed_image_omission_names_existing_artifact(binary: Path) -> Transcript:
    with recovery_client(binary) as (client, profile, reached, release):
        for _ in range(2):
            pending = client.start_send(
                control="interrupt", r="preview allocation image and text"
            )
            cancel_result(client, pending, reached, release)
        profile.pause_results(False)
        result = client.send(control="interrupt")
        text = "".join(
            block["text"] for block in result["content"] if block["type"] == "text"
        )
        assert text.count("[output omitted:") == 1, text
        assert "1 image" in text and "retained output:" in text
        session = session_directory(client)
        assert text.count(f".agents/console/sessions/{session.name}") == 1
        assert "logs: capture/sessions/001/" in text
        artifacts = sorted((session / "capture/sessions/001").glob("*.png"))
        assert len(artifacts) == 2, artifacts
        omitted = artifacts[1]
        assert omitted.read_bytes() == bytes(6 * 1024 * 1024)
        assert f"images: capture/sessions/001/{omitted.name}" in text
        events = live_recording_events(session)
        summaries = {
            event["call_id"]: event
            for event in events
            if event["event"] == "cell_output"
        }
        marker = re.search(r"output omitted: (\d+) UTF-8 bytes", text)
        assert marker is not None
        assert sum(
            event["inline_omitted_bytes"] for event in summaries.values()
        ) == int(marker[1])
        for call_id in (2, 3):
            assert (
                cell_text(client, call_id)
                == "preview head\n" + "x" * 32768 + "\npreview tail\n"
            )
        assert sum(block["type"] == "image" for block in result["content"]) == 1
        assert recorded_omission_owners(client) == [None]
        client.send(session="default")
        assert last_tool_text(client) == "\n[idle]"
        client.finish()
    return [
        {
            "composed_image_omission_has_readable_artifact": True,
            "accepted_image_preserved": True,
            "poll_consumed_notice": True,
        }
    ]


def recorded_omission_owners(client: McpClient) -> list[dict | None]:
    result = next(
        event
        for event in reversed(live_recording_events(session_directory(client)))
        if event["event"] == "tool_result"
    )
    return [
        part["owner"]
        for part in result["parts"]
        if "output omitted:"
        in result["result"]["content"][part["block"]]
        .get("text", "")
        .encode()[part["start"] : part["end"]]
        .decode()
    ]


def composed_image_omission_ownership(
    binary: Path, *, count: int, text: bool = False
) -> Transcript:
    with recovery_client(binary) as (client, profile, reached, release):
        for index in range(count):
            pending = client.start_send(
                control="interrupt",
                r="preview allocation image and text"
                if text and index
                else "preview allocation image",
            )
            cancel_result(client, pending, reached, release)
        profile.pause_results(False)
        result = client.send(control="interrupt")
        assert not result["isError"], result
        assert sum(block["type"] == "image" for block in result["content"]) == 1
        expected = (
            {"session_id": 1, "cell_id": 3, "interval": "cell"} if count == 2 else None
        )
        assert recorded_omission_owners(client) == [expected]
        client.finish()
    return [{"omitted_images": count - 1, "owner": expected}]


@requires(NATIVE_FIXTURES)
def test_composed_image_omission_preserves_single_interval(binary: Path) -> Transcript:
    return composed_image_omission_ownership(binary, count=2)


@requires(NATIVE_FIXTURES)
def test_composed_text_and_image_omission_preserves_single_interval(
    binary: Path,
) -> Transcript:
    return composed_image_omission_ownership(binary, count=2, text=True)


@requires(NATIVE_FIXTURES)
def test_composed_image_omission_clears_mixed_intervals(binary: Path) -> Transcript:
    return composed_image_omission_ownership(binary, count=3)


@requires(NATIVE_FIXTURES)
def test_long_recording_paths_keep_image_omissions_readable(binary: Path) -> Transcript:
    for mixed in (False, True):
        with tempfile.TemporaryDirectory() as temporary:
            recording = Path(temporary).joinpath(*(["recording-" + "é" * 70] * 4))
            recording.mkdir(parents=True)
            with recovery_client(binary, recording_directory=recording) as (
                client,
                profile,
                reached,
                release,
            ):
                for _ in range(2):
                    pending = client.start_send(
                        control="interrupt",
                        r=(
                            "preview allocation image and text"
                            if mixed
                            else "preview allocation image"
                        ),
                    )
                    cancel_result(client, pending, reached, release)
                profile.pause_results(False)
                result = client.send(control="interrupt")
                assert not result["isError"], result
                text = "".join(
                    block["text"]
                    for block in result["content"]
                    if block["type"] == "text"
                )
                assert len(text.encode()) <= TEXT_BUDGET
                assert text.count("[output omitted:") == 1, text
                session = next((recording / "sessions").iterdir())
                artifacts = sorted((session / "capture/sessions/001").glob("*.png"))
                assert len(artifacts) == 2, artifacts
                omitted = artifacts[1]
                assert omitted.read_bytes() == bytes(6 * 1024 * 1024)
                if mixed:
                    location = (
                        f"retained output: sessions/{session.name}/"
                        " (relative to Console recording directory)"
                    )
                    assert location in text
                    assert "logs: capture/sessions/001/" in text
                    assert f"images: capture/sessions/001/{omitted.name}" in text
                else:
                    marker = re.search(r"\n\[output omitted: [^\n]*\]\n", text)
                    assert marker is not None, text
                    _, advertised = marker[0].split("; retained image: ", 1)
                    suffix = " (relative to Console recording directory)]\n"
                    assert advertised.endswith(suffix)
                    assert recording / advertised.removesuffix(suffix) == omitted
                assert str(recording) not in text
                assert sum(block["type"] == "image" for block in result["content"]) == 1
                client.send(session="default")
                assert last_tool_text(client) == "\n[idle]"
                client.finish()
    return [
        {
            "long_path_image_only_omission_readable": True,
            "long_path_mixed_omission_readable": True,
            "complete_text_budget_preserved": True,
            "poll_consumed_notice": True,
        }
    ]


@requires(NATIVE_FIXTURES)
def test_unretained_image_omission_preserves_interval(binary: Path) -> Transcript:
    return recovered_image_omissions(binary, count=1)


@requires(NATIVE_FIXTURES)
def test_recovered_image_omissions_are_aggregated(binary: Path) -> Transcript:
    return recovered_image_omissions(binary, count=4)


@requires(NATIVE_FIXTURES)
def test_recovered_image_omissions_keep_bounded_state(binary: Path) -> Transcript:
    return recovered_image_omissions(binary, count=1024, allocation_limit=128 * 1024)


def recovered_image_omissions(
    binary: Path, *, count: int, allocation_limit: int | None = None
) -> Transcript:
    with recovery_client(binary) as (client, profile, reached, release):
        # The first cell has no cancelled response to hand off.
        pending = client.start_send(r="preview rejected image")
        cancel_result(client, pending, reached, release)
        start = len(client.transcript)
        for _ in range(count - 1):
            # Handoff carries the cancelled result into the next cell while
            # reusing the worker, preserving all image-admission cycles.
            pending = client.start_send(control="interrupt", r="preview rejected image")
            cancel_result(client, pending, reached, release)
        if count > 1:
            compact_cancelled_exchanges(
                client,
                start,
                count=count - 1,
                reply=lambda index: (
                    f"\n[output omitted: {index + 2} images "
                    f"({4 * (index + 2)} encoded bytes); "
                    f"{index + 2} images not retained]\n\n"
                    + "\n".join(["[done]"] * (index + 1))
                ),
            )
        profile.pause_results(False)
        # Measure recovery after image parsing and admission have completed.
        profile.start()
        result = client.send(control="interrupt")
        client.request("ping")
        _, largest = profile.stop()
        if allocation_limit is not None:
            assert largest <= allocation_limit, largest
        assert not result["isError"], result
        text = result["content"][0]["text"]
        assert text.count("[output omitted:") == 1
        noun = "image" if count == 1 else "images"
        assert (
            f"output omitted: {count} {noun} ({4 * count} encoded bytes); "
            f"{count} {noun} not retained"
        ) in text
        expected = (
            {"session_id": 1, "cell_id": 2, "interval": "cell"} if count == 1 else None
        )
        assert recorded_omission_owners(client) == [expected]
        client.send(session="default")
        assert last_tool_text(client) == "\n[idle]"
        compact_previews(client, "\n[done]")
        return client.finish()


if __name__ == "__main__":
    run_this_suite(__file__)
