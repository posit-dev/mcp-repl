"""Control previews retain image receipts without changing delivery accounting."""

import base64
import json
import os
import re
import sys
import tempfile
from contextlib import ExitStack, closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.integrations.test_control import (
    ControlClient,
    result_text,
    wait_for_queued_integrator,
)
from boundaries.client_server.python.test_without_r import selected_environment
from support.assertions import assert_result_content
from support.checkpoints import FifoCheckpoint, wait_for_checkpoint
from support.client import McpClient
from support.evidence import compact_text
from support.execution import DIRECT, SANDBOXED, Execution, executions
from support.normalization import code, normalize_recording
from support.native import LOADER_VARIABLE, build_interposer
from support.previews import OMISSION, assert_preview, recording_events
from support.records import Transcript, TranscriptWithCompanions
from support.requirements import NATIVE_FIXTURES, POSIX, R, requires
from support.suites import run_this_suite


@requires(NATIVE_FIXTURES, POSIX, R)
@executions(DIRECT, SANDBOXED)
def test_previews_images_without_consuming_or_reaccounting(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-images-"
    ) as directory:
        root = Path(directory)
        endpoint = root / "control.sock"
        arguments = ["--control-socket", str(endpoint)]
        if execution == SANDBOXED:
            arguments += ["--writable-root", str(root)]
        environment = dict(
            os.environ,
            MCP_CONSOLE_TEST_ACK_REACHED=str(root / "ack-reached"),
            MCP_CONSOLE_TEST_ACK_RELEASE=str(root / "ack-release"),
        )
        environment[LOADER_VARIABLE] = str(
            build_interposer(root, "control_response_ack")
        )
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            closing(FifoCheckpoint.create(root / "ack-reached")) as ack_reached,
            closing(FifoCheckpoint.create(root / "ack-release")) as ack_release,
            McpClient(
                binary,
                execution.serve(*arguments),
                environment,
                current_directory=root,
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.expect(r="invisible(NULL)")
            # fmt: r
            source = code(f"""
                plot(1:3)
                invisible(grDevices::dev.off())
                cat(strrep("x", 24000), "\\n", sep = "")
                gate <- fifo(
                  {
                    json.dumps(str(entered.path))
                  },
                  "wb",
                  blocking = TRUE
                )
                writeBin(charToRaw("1"), gate)
                close(gate)
                gate <- fifo(
                  {
                    json.dumps(str(release.path))
                  },
                  "rb",
                  blocking = TRUE
                )
                invisible(readBin(gate, "raw", n = 1))
                close(gate)
                """)
            pending = client.start_send(r=source, timeout_ms=60000)
            try:
                entered.wait(client=client)
                (recording,) = (root / ".agents/console/sessions").iterdir()
                emitted = "x" * 24000 + "\n"

                def captured() -> Path | None:
                    journal = (recording / "capture/events.jsonl").read_bytes()
                    events = [
                        json.loads(line)
                        for line in journal.splitlines(keepends=True)
                        if line.endswith(b"\n")
                    ]
                    if not any(event["event"] == "image_retained" for event in events):
                        return None
                    return next(
                        (
                            path
                            for path in (recording / "capture/sessions").glob(
                                "*/cell-*.txt"
                            )
                            if path.read_text() == emitted
                        ),
                        None,
                    )

                wait_for_checkpoint(
                    captured,
                    "agent plot and complete text captured before control preview",
                    root=recording,
                    recursive=True,
                    watch_files=lambda: (
                        recording / "capture/events.jsonl",
                        *(recording / "capture/sessions").glob("*/cell-*.txt"),
                    ),
                    client=client,
                )
                with ControlClient(endpoint) as control:
                    previews = [control.request(op="preview") for _ in range(2)]
                    assert previews[0]["result"] == previews[1]["result"], previews
                    content = previews[0]["result"]["content"]
                    assert [block["type"] for block in content] == ["image", "text"]
                    assert_preview(content[1]["text"], emitted)
                    public_image = base64.b64decode(content[0]["data"], validate=True)
                    public_encoded = content[0]["data"]
                    release.release()
                    # The held write returns only on release, so the recording
                    # cannot yet hold this reply's delivery.
                    client.recording_receipts = False
                    client.receive(pending)
                    client.recording_receipts = True
                    ack_reached.wait(
                        "complete MCP response written before acknowledgment"
                    )
                    result = pending["result"]["content"]
                    assert [block["type"] for block in result] == ["image", "text"]
                    omitted = assert_preview(result[1]["text"], emitted)
                    assert_result_content(
                        client,
                        [public_image, result[1]["text"]],
                        image_reference="captured agent plot",
                    )
                    hidden_source = "plot(3:1); invisible(grDevices::dev.off())"
                    # Receiving the MCP frame does not acknowledge its delivery
                    # inside the server. Queue behind that retained owner.
                    hidden_call = control.start(
                        op="evaluate",
                        language="r",
                        code=hidden_source,
                        record=False,
                        busy="wait",
                        wait_ms=30000,
                    )
                    wait_for_queued_integrator(client, root, hidden_source)
                    ack_release.release()
                    hidden = control.receive(hidden_call)
                    assert "result" in hidden, hidden
                    (hidden_block,) = hidden["result"]["content"]
                    assert hidden_block["type"] == "image", hidden
                    hidden_encoded = hidden_block["data"]
                    hidden_image = base64.b64decode(hidden_encoded, validate=True)
                    client.expect("still recorded\n", r='cat("still recorded\\n")')
                    transcript, stderr = client.finish_with_standard_error()
                    assert stderr == "", stderr

                events = recording_events(recording)
                assert events[-1]["event"] == "recording_ended", events[-1]
                (source_call,) = [
                    event["call_id"]
                    for event in events
                    if event["event"] == "tool_call"
                    and event["request"].get("arguments", {}).get("r") == source
                ]
                summaries = [
                    event
                    for event in events
                    if event["event"] == "cell_output"
                    and event["call_id"] == source_call
                ]
                assert summaries, events
                assert summaries[-1]["inline_omitted_bytes"] == omitted, summaries
                public_artifact, hidden_artifact = [
                    event for event in events if event["event"] == "image_retained"
                ]
                assert (
                    recording / public_artifact["path"]
                ).read_bytes() == public_image
                assert (
                    recording / hidden_artifact["path"]
                ).read_bytes() == hidden_image
                assert hidden_artifact["owner"]["omit_from_projections"] is True
                markdown = (recording / "transcript.md").read_text()
                assert public_artifact["path"] in markdown, markdown
                assert hidden_artifact["path"] not in markdown, markdown
                (qmd,) = (recording / "sessions").glob("*.qmd")
                assert hidden_source not in qmd.read_text(), qmd.read_text()
                journal = (recording / "capture/events.jsonl").read_text()
                assert public_encoded not in journal and hidden_encoded not in journal
                assert "still recorded" in journal, journal
                for entry in control.transcript:
                    for block in entry["response"]["result"]["content"]:
                        if block["type"] == "image":
                            label = (
                                "agent"
                                if block["data"] == public_encoded
                                else "integrator"
                            )
                            block["data"] = (
                                f"<PNG byte-identical to captured {label} plot>"
                            )
                        else:
                            block["text"] = compact_text(block["text"], "x")
                transcript = normalize_recording(transcript, root)
                for entry in transcript:
                    for block in entry.get("result", {}).get("content", []):
                        if block["type"] == "text":
                            block["text"] = compact_text(block["text"], "x")
                return TranscriptWithCompanions(
                    transcript,
                    {"control.yaml": normalize_recording(control.transcript, root)},
                )
            finally:
                release.release()
                ack_release.release()


@requires(POSIX, R)
@executions(DIRECT, SANDBOXED)
def test_preview_qualifies_retained_files_after_recording_stops(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-stopped-"
    ) as directory:
        root = Path(directory)
        endpoint = root / "control.sock"
        arguments = ["--control-socket", str(endpoint)]
        if execution == SANDBOXED:
            arguments += ["--writable-root", str(root)]
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            McpClient(
                binary, execution.serve(*arguments), current_directory=root
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.expect(r="invisible(NULL)")
            (recording,) = (root / ".agents/console/sessions").iterdir()
            # The recording writer cannot create the plot's file. Its failure
            # stops recording after the cell's text file was named.
            (recording / "capture/sessions/001/cell-002-001.png").mkdir()
            # fmt: r
            source = code(f"""
                cat(strrep("x", 24000), "\\n", sep = "")
                plot(1:3)
                invisible(grDevices::dev.off())
                gate <- fifo(
                  {
                    json.dumps(str(entered.path))
                  },
                  "wb",
                  blocking = TRUE
                )
                writeBin(charToRaw("1"), gate)
                close(gate)
                gate <- fifo(
                  {
                    json.dumps(str(release.path))
                  },
                  "rb",
                  blocking = TRUE
                )
                invisible(readBin(gate, "raw", n = 1))
                close(gate)
                """)
            pending = client.start_send(r=source, timeout_ms=60000)
            try:
                entered.wait(client=client)
                client.await_recording_disabled()
                stopped = (
                    "; recording stopped; retained files may be missing or incomplete]"
                )

                def marker(content: list[dict]) -> str:
                    text = "".join(
                        block["text"] for block in content if block["type"] == "text"
                    )
                    (line,) = [
                        line
                        for line in text.splitlines()
                        if line.startswith("[output omitted: ")
                    ]
                    assert "cell-002.txt" in line, line
                    return line

                with ControlClient(endpoint) as control:
                    preview = control.request(op="preview")
                    assert marker(preview["result"]["content"]).endswith(stopped), (
                        preview
                    )
                release.release()
                client.receive(pending)
                assert marker(pending["result"]["content"]).endswith(stopped), pending
                _, stderr = client.finish_with_standard_error()
                assert stderr.count("transcript recording disabled:") == 1, stderr
            finally:
                release.release()
    return [
        {
            "preview_names_raw_log": True,
            "preview_qualifies_retained_files_after_recording_stops": True,
            "reply_carries_the_same_qualifier": True,
        }
    ]


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_preview_does_not_publish_omission_for_completed_intervals(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with (
        tempfile.TemporaryDirectory(
            dir="/tmp", prefix="console-control-preview-"
        ) as directory,
        ExitStack() as resources,
    ):
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        gates = [
            resources.enter_context(closing(FifoCheckpoint.create(root / f"gate-{n}")))
            for n in range(3)
        ]
        arguments = ["--control-socket", str(endpoint)]
        if execution == SANDBOXED:
            arguments += ["--writable-root", str(root)]
        client = resources.enter_context(
            McpClient(
                binary, execution.serve(*arguments), selected_environment(root), root
            )
        )
        for gate in gates:
            resources.callback(gate.release)
        client.initialize_and_list_tools()
        # fmt: python
        source = code(f"""
            import threading

            def emit_after_gates():
                for path, text in zip({[str(gate.path) for gate in gates]!r}, ["a" * 5000, "b" * 100, "c" * 20000]):
                    with open(path, "rb", buffering=0) as gate:
                        gate.read(1)
                    print(text, end="", flush=True)

            producer = threading.Thread(target=emit_after_gates)
            producer.start()
            """)
        client.expect(python=source)
        (recording,) = (root / ".agents/console/sessions").iterdir()
        texts = ["a" * 5000, "b" * 100, "c" * 20000]
        paths = []
        with ControlClient(endpoint) as control:
            for index, (gate, text) in enumerate(zip(gates, texts)):
                gate.release()

                def captured() -> Path | None:
                    return next(
                        (
                            path
                            for path in (recording / "capture/sessions").glob(
                                "*/cell-*-post.txt"
                            )
                            if path.read_text() == text
                        ),
                        None,
                    )

                path = wait_for_checkpoint(
                    captured,
                    "background output retained in its observed post-cell interval",
                    root=recording,
                    recursive=True,
                    watch_files=lambda: (recording / "capture/sessions").glob(
                        "*/cell-*-post.txt"
                    ),
                    client=client,
                )
                paths.append(path)
                if index < 2:
                    assert (
                        result_text(
                            control.request(
                                op="evaluate",
                                language="python",
                                code="pass",
                                record=False,
                                busy="wait",
                                wait_ms=30000,
                            )
                        )
                        == "[done]"
                    )
            middle = str(paths[1].relative_to(recording))
            before = [
                event
                for event in recording_events(recording)
                if event["event"] == "session_output" and event["path"] == middle
            ]
            assert before and before[-1]["inline_omitted_bytes"] == 0, before
            previews = [control.request(op="preview") for _ in range(2)]
            assert previews[0]["result"] == previews[1]["result"], previews
            after = [
                event
                for event in recording_events(recording)
                if event["event"] == "session_output" and event["path"] == middle
            ]
            assert all(event["inline_omitted_bytes"] == 0 for event in after), after
            result = client.send(session="default")
            # The combined count covers three interval owners, whose distinct
            # gaps retain two additional markers before the visible suffix.
            assert_preview(
                result["content"][0]["text"].removesuffix("\n[idle]"),
                "".join(texts),
                pattern=re.compile(OMISSION.pattern + r"\n\[…\]\n\n\[…\]\n"),
            )
            transcript = normalize_recording(client.finish(), root)
        summaries = [
            event
            for event in recording_events(recording)
            if event["event"] == "session_output" and event["path"] == middle
        ]
        assert summaries[-1]["inline_omitted_bytes"] == len(texts[1]), summaries
        for entry in transcript:
            for block in entry.get("result", {}).get("content", []):
                if block["type"] == "text":
                    block["text"] = compact_text(block["text"], "a", "b", "c")
        for entry in control.transcript:
            for block in entry["response"]["result"]["content"]:
                if block["type"] == "text":
                    block["text"] = compact_text(block["text"], "a", "b", "c")
        return TranscriptWithCompanions(
            transcript, {"control.yaml": normalize_recording(control.transcript, root)}
        )


if __name__ == "__main__":
    run_this_suite(__file__)
