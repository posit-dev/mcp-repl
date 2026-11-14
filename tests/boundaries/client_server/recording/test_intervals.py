"""Observed capture intervals and managed file completion through MCP."""

import base64
import json
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from support.client import McpClient
from support.previews import live_recording_events
from support.checkpoints import release_fixture_checkpoint, wait_for_checkpoint
from support.normalization import normalize_recording
from boundaries.client_server._harness import wait_for_marker
from support.execution import DIRECT, RUNTIME, SANDBOXED, Execution, executions
from support.requirements import POSIX, requires
from support.records import Transcript
from support.suites import run_this_suite


def events(recording: Path) -> list[dict]:
    return [
        json.loads(line)
        for line in (recording / "capture/events.jsonl").read_text().splitlines()
    ]


@executions(RUNTIME)
def test_prompt_only_fragments_keep_cell_ownership(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as directory:
        workspace = Path(directory)
        with McpClient(
            binary,
            execution.serve(
                "-c", "r=null", "-c", f"python={json.dumps(sys.executable)}"
            ),
            {**os.environ, "RETICULATE_PYTHON": sys.executable},
            current_directory=workspace,
        ) as client:
            client.initialize_and_list_tools()
            for source in (
                'answer = input("x> ")',
                'print("before"); answer = input("x> ")',
            ):
                result = client.send(python=source, timeout_ms=60000)
                assert "x> " in result["content"][0]["text"], result
                client.expect(stdin="answer\n")
            transcript = client.finish()
        (recording,) = (workspace / ".agents/console/sessions").iterdir()
        prompts = []
        for event in events(recording):
            if event["event"] != "tool_result":
                continue
            for part in event["parts"]:
                block = event["result"]["content"][part["block"]]
                if block["type"] == "text":
                    text = block["text"].encode()[part["start"] : part["end"]].decode()
                    if "x> " in text:
                        prompts.append(part["owner"])
        assert prompts == [
            {"session_id": 1, "cell_id": cell, "interval": "cell"} for cell in (1, 2)
        ], prompts
        return normalize_recording(transcript, workspace)


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_preview_omission_preserves_raw_images(
    binary: Path, execution: Execution
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        workspace = Path(directory)
        with McpClient(
            binary,
            execution.serve("--worker", str(worker)),
            current_directory=workspace,
        ) as client:
            client.initialize_and_list_tools()
            result = client.send(r="preview image limit")
            assert not result["isError"], result
            (image,) = [b for b in result["content"] if b["type"] == "image"]
            (recording,) = (workspace / ".agents/console/sessions").iterdir()
            capture = live_recording_events(recording)
            artifacts = [e for e in capture if e["event"] == "image_retained"]
            assert len(artifacts) == 2, artifacts
            omitted, returned = artifacts
            assert (recording / omitted["path"]).read_bytes() == b"\0" * (
                6 * 1024 * 1024 + 3
            )
            assert (recording / returned["path"]).read_bytes() == base64.b64decode(
                image["data"]
            )
            text = "".join(b["text"] for b in result["content"] if b["type"] == "text")
            assert "retained image:" in text and omitted["path"] in text, text
            assert "not retained" not in text, text
            assert not any(e["event"] == "image_omitted" for e in capture)
            recorded = next(e for e in reversed(capture) if e["event"] == "tool_result")
            owners = [
                part["owner"]
                for part in recorded["parts"]
                if "output omitted:"
                in recorded["result"]["content"][part["block"]]
                .get("text", "")
                .encode()[part["start"] : part["end"]]
                .decode()
            ]
            assert owners == [{"session_id": 1, "cell_id": 1, "interval": "cell"}], (
                owners
            )
            client.send(session="default")
            assert (recording / omitted["path"]).read_bytes() == b"\0" * (
                6 * 1024 * 1024 + 3
            )
            transcript = client.finish()
        assert all(
            (recording / a["path"]).stat().st_mode & 0o222 == 0 for a in artifacts
        )
        return normalize_recording(transcript, workspace)


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_raw_image_limit_preserves_inline_output_and_resets_at_cell_boundary(
    binary: Path, execution: Execution
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        workspace = Path(directory)
        with McpClient(
            binary,
            execution.serve("--worker", str(worker)),
            current_directory=workspace,
        ) as client:
            client.initialize_and_list_tools()
            result = client.send(r="overflow image retention")
            assert not result["isError"], result
            (image,) = [b for b in result["content"] if b["type"] == "image"]
            assert base64.b64decode(image["data"]) == b"\0" * (4 * 1024 * 1024)
            image["data"] = "<4 MiB of zero bytes, verified byte-for-byte>"
            (recording,) = (workspace / ".agents/console/sessions").iterdir()
            capture = live_recording_events(recording)
            artifacts = [e for e in capture if e["event"] == "image_retained"]
            assert len(artifacts) == 10, len(artifacts)
            assert sum(
                (recording / a["path"]).stat().st_size for a in artifacts
            ) == 10 * (6 * 1024 * 1024 + 3)
            (omitted,) = [e for e in capture if e["event"] == "image_omitted"]
            owner = {"session_id": 1, "cell_id": 1, "interval": "cell"}
            assert omitted["owner"] == owner
            assert omitted["encoded_bytes"] == 5592408
            (recorded,) = [e for e in capture if e["event"] == "tool_result"]
            (block,) = [
                b for b in recorded["result"]["content"] if b["type"] == "image"
            ]
            assert block == {
                "type": "image",
                "mimeType": "image/png",
                "retained": False,
            }, block
            assert (
                next(
                    p
                    for p in recorded["parts"]
                    if recorded["result"]["content"][p["block"]]["type"] == "image"
                )["owner"]
                == owner
            )
            client.send(r="preview image limit")
            transcript = client.finish()
        capture = events(recording)
        assert capture[-1]["event"] == "recording_ended"
        assert "_Image not retained._" in (recording / "transcript.md").read_text()
        assert len([e for e in capture if e["event"] == "image_omitted"]) == 1
        assert (
            len(
                [
                    e
                    for e in capture
                    if e["event"] == "image_retained" and e["owner"]["cell_id"] == 2
                ]
            )
            == 2
        )
        return normalize_recording(transcript, workspace)


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_retains_one_image_at_the_raw_image_limit(
    binary: Path, execution: Execution
) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        workspace = Path(directory)
        with McpClient(
            binary,
            execution.serve("--worker", str(worker)),
            current_directory=workspace,
        ) as client:
            client.initialize_and_list_tools()
            # One image may use the whole interval budget. Recording it must
            # leave room for the events that complete its cell.
            result = client.send(r="emit largest retained image")
            assert not result["isError"], result
            (recording,) = (workspace / ".agents/console/sessions").iterdir()
            transcript = client.finish()
        capture = events(recording)
        (artifact,) = [e for e in capture if e["event"] == "image_retained"]
        assert (recording / artifact["path"]).stat().st_size == 64 * 1024 * 1024
        assert not [e for e in capture if e["event"] == "image_omitted"]
        assert capture[-1]["event"] == "recording_ended"
        return normalize_recording(transcript, workspace)


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_cell_intervals_and_empty_cells(
    binary: Path, execution: Execution
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        workspace = Path(directory)
        with McpClient(
            binary,
            execution.serve("--worker", str(zod)),
            {**os.environ, "TMPDIR": directory, "ZOD_RECORDING_INTERVALS": "1"},
            current_directory=workspace,
        ) as client:
            client.initialize_and_list_tools()
            client.send(r="record intervals")
            post = wait_for_marker(workspace, "zod-recording-post-release", client)
            release_fixture_checkpoint(post, client=client)
            wait_for_marker(workspace, "zod-recording-post-observed", client)
            client.send(session="default")
            (active_recording,) = (workspace / ".agents/console/sessions").iterdir()
            active_capture = active_recording / "capture/sessions/001"
            capture = events(active_recording)
            assert not any(e["event"] == "source_attached" for e in capture)
            assert [
                e["session_id"]
                for e in capture
                if e["event"] == "source_associated" and e["call_id"] == 1
            ] == [1]
            assert (active_capture / "cell-001.txt").stat().st_mode & 0o222 == 0
            assert (active_capture / "cell-001-post.txt").stat().st_mode & 0o200
            assert (active_recording / "capture/events.jsonl").stat().st_mode & 0o200
            for image in active_capture.glob("*.png"):
                assert image.stat().st_mode & 0o222 == 0, image
            results = [
                event
                for event in events(active_recording)
                if event["event"] == "tool_result"
            ]
            startup = {"session_id": 1, "cell_id": None, "interval": "startup"}
            cell = {"session_id": 1, "cell_id": 1, "interval": "cell"}
            post_owner = {"session_id": 1, "cell_id": 1, "interval": "post"}
            observed = []
            for result in results:
                fragments = []
                for part in result["parts"]:
                    block = result["result"]["content"][part["block"]]
                    if block["type"] == "text":
                        text = (
                            block["text"].encode()[part["start"] : part["end"]].decode()
                        )
                    else:
                        assert part["start"] == part["end"] == 0
                        assert (active_recording / block["path"]).is_file()
                        text = "image"
                    fragments.append((text, part["owner"]))
                observed.append(fragments)
            assert observed == [
                [
                    ("startup text\n", startup),
                    ("image", startup),
                    ("before image\n", cell),
                    ("image", cell),
                    ("after image\n", cell),
                    ("image", cell),
                ],
                [
                    ("between cells\n", post_owner),
                    ("image", post_owner),
                    ("\n[idle]", None),
                ],
            ], observed
            client.send(r="complete silently")
            assert (active_capture / "cell-001-post.txt").stat().st_mode & 0o222 == 0
            client.request(
                "tools/call",
                name="send",
                arguments={"r": "rejected source", "typo": True},
            )
            client.send(control="restart", r="complete silently")
            restart = [
                event
                for event in events(active_recording)
                if event["event"] == "tool_result"
            ][-1]
            assert {
                part["owner"]["session_id"]
                for part in restart["parts"]
                if part["owner"] is not None
            } == {1, 2}, restart
            for name in ("transcript.md", "sessions/session-002.qmd"):
                assert (active_recording / name).stat().st_mode & 0o200
            assert (
                active_recording / "sessions/session-001.qmd"
            ).stat().st_mode & 0o200
            transcript = client.finish()
        (recording,) = (workspace / ".agents/console/sessions").iterdir()
        capture = recording / "capture/sessions/001"
        assert (capture / "startup.txt").read_text() == "startup text\n"
        assert (capture / "cell-001.txt").read_text() == "before image\nafter image\n"
        assert (capture / "cell-001-post.txt").read_text() == "between cells\n"
        assert (capture / "cell-002-post.txt").read_text() == "shutdown text\n"
        for name in [
            "startup-001.png",
            "cell-001-002.png",
            "cell-001-post-001.png",
            "cell-002-post-001.png",
        ]:
            assert (capture / name).is_file()
        interleaving = []
        for event in events(recording):
            if event["event"] not in {"output_text", "image_retained"} or event[
                "owner"
            ] != {"session_id": 1, "cell_id": 1, "interval": "cell"}:
                continue
            if event["event"] == "output_text":
                raw = (recording / event["path"]).read_bytes()
                interleaving.append(
                    raw[event["offset"] : event["offset"] + event["bytes"]].decode()
                )
            else:
                interleaving.append("image")
        assert interleaving == ["before image\n", "image", "after image\n", "image"]
        assert (capture / "cell-001-001.png").is_file()
        assert (capture / "cell-002.txt").read_bytes() == b""
        assert not (capture / "cell-003.txt").exists()
        assert (recording / "capture/sessions/002/cell-001.txt").read_bytes() == b""
        assert "rejected source" in (recording / "sessions/session-001.qmd").read_text()
        assert all(e["sequence"] == i + 1 for i, e in enumerate(events(recording)))
        for path in recording.rglob("*"):
            if path.is_file():
                assert path.stat().st_mode & 0o222 == 0, path
                if os.name != "nt":
                    assert path.stat().st_mode & 0o077 == 0, path
            elif path.is_dir():
                assert path.stat().st_mode & 0o200, path
        return normalize_recording(transcript, workspace)


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_sessions_without_cells_keep_shutdown_in_startup(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as directory:
        workspace = Path(directory)
        zod = Path(__file__).resolve().parents[3] / "fixtures/zod"
        with McpClient(
            binary,
            execution.serve("--worker", str(zod)),
            {
                **os.environ,
                "ZOD_RECORDING_INTERVALS": "1",
                "ZOD_RECORDING_STARTUP": "1",
            },
            current_directory=workspace,
        ) as client:
            client.initialize_and_list_tools()
            client.send(control="restart", timeout_ms=60000)
            (recording,) = (workspace / ".agents/console/sessions").iterdir()
            first = recording / "capture/sessions/001/startup.txt"
            wait_for_checkpoint(
                lambda: (
                    first
                    if first.exists() and first.read_text() == "startup text\n"
                    else None
                ),
                "startup text observed before a cell executes",
                root=first,
                client=client,
            )
            assert first.stat().st_mode & 0o200
            client.send(control="restart", timeout_ms=60000)
            assert first.read_text() == "startup text\nshutdown text\n"
            assert first.stat().st_mode & 0o222 == 0
            second = recording / "capture/sessions/002/startup.txt"
            wait_for_checkpoint(
                lambda: (
                    second
                    if second.exists() and second.read_text() == "startup text\n"
                    else None
                ),
                "replacement startup text observed without a cell",
                root=second,
                client=client,
            )
            client.send(stdin="unused input")
            transcript = client.finish()
        capture = events(recording)
        assert [e["session_id"] for e in capture if e["event"] == "worker_started"] == [
            1,
            2,
        ]
        assert not any(e["event"] == "cell_started" for e in capture)
        assert not list((recording / "sessions").glob("*.qmd"))
        assert sorted(
            p.relative_to(recording).as_posix() for p in recording.rglob("*.txt")
        ) == ["capture/sessions/001/startup.txt", "capture/sessions/002/startup.txt"]
        assert (
            recording / "capture/sessions/002/startup.txt"
        ).read_text() == "startup text\nshutdown text\n"
        for path in recording.rglob("*"):
            if path.is_file():
                assert path.stat().st_mode & 0o222 == 0, path
        return [
            *transcript[:3],
            {
                "capture": "sessions without cells retain startup and shutdown in startup intervals"
            },
        ]


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_projection_failure_preserves_capture_and_finalizes_files(
    binary: Path, execution: Execution
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as directory:
        workspace = Path(directory)
        with McpClient(
            binary, execution.serve("--worker", str(zod)), current_directory=workspace
        ) as client:
            client.initialize_and_list_tools()
            client.expect("zod: first\n", r="echo first")
            (recording,) = (workspace / ".agents/console/sessions").iterdir()
            qmd = recording / "sessions/session-001.qmd"
            qmd.unlink()
            qmd.mkdir()
            client.expect("zod: second\n", r="echo second")
            client.expect("zod: third\n", r="echo third")
            transcript, stderr = client.finish_with_standard_error()
        assert stderr.count("transcript projections disabled:") == 1, stderr
        assert "transcript recording disabled:" not in stderr, stderr
        assert events(recording)[-1]["event"] == "recording_ended"
        assert (
            recording / "capture/sessions/001/cell-002.txt"
        ).read_text() == "zod: second\n"
        assert (
            recording / "capture/sessions/001/cell-003.txt"
        ).read_text() == "zod: third\n"
        for path in recording.rglob("*"):
            if path.is_file():
                assert path.stat().st_mode & 0o222 == 0, path
        return normalize_recording(transcript, workspace)


if __name__ == "__main__":
    run_this_suite(__file__)
