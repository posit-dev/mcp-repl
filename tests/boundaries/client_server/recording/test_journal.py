#!/usr/bin/env -S uv run --script

import base64
import json
import os
import shutil
import subprocess
import sys
import tempfile
from contextlib import ExitStack, closing, contextmanager
from datetime import datetime
from pathlib import Path

from yaml12 import format_yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from support.previews import cell_output_path, live_recording_events
from support.progress import normalize_elapsed, without_elapsed_result
from support.requirements import (
    NATIVE_FIXTURES,
    POSIX,
    PROCESS_EVENTS,
    R,
    SQL,
    command,
    requires,
)
from support.assertions import last_result_text, last_tool_text
from support.checkpoints import FifoCheckpoint, wait_for_checkpoint
from support.native import LOADER_VARIABLE, build_interposer
from support.normalization import code
from support.client import McpClient, stop_client
from support.execution import DIRECT, SANDBOXED, Execution, executions
from support.r import r_test_environment, reference_plots, startup_r_package
from support.records import Transcript, TranscriptWithCompanions
from support.resolvers import bare_runtime_environment, record_resolved_r_library
from support.suites import run_this_suite
from support.discovery import discovery_environment


def call_events(events: list[dict]) -> list[dict]:
    """The canonical call/output records, excluding protocol and lifecycle evidence."""
    return [
        event
        for event in events
        if event["event"]
        in {
            "recording_started",
            "tool_call",
            "tool_result",
            "image_retained",
            "cell_output",
        }
    ]


CELL_OUTPUT_RETENTION_LIMIT = 1024 * 1024 * 1024


PNG_1X1 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42Y"
    "AAAAASUVORK5CYII="
)

from boundaries.client_server._harness import wait_for_marker


STARTUP_PLOT = (
    # fmt: r
    code("""
        options(
          console.plot.width_in = 4,
          console.plot.height_in = 3,
          console.plot.dpi = 100
        )
        graphics::plot(1:3)
        grDevices::dev.off()
        """)
)


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_materializes_records_only_for_console_use(
    binary: Path, execution: Execution
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    with tempfile.TemporaryDirectory() as temporary_directory:
        temporary = Path(temporary_directory)
        unused_workspace = temporary / "unused"
        unused_workspace.mkdir()
        client = McpClient(
            binary,
            execution.serve("--worker", str(zod)),
            {**os.environ, "TMPDIR": str(unused_workspace)},
            current_directory=unused_workspace,
            record_in_project=False,
        )
        client.initialize_and_list_tools()
        assert not (unused_workspace / ".agents").exists(), unused_workspace
        removed = client.request(
            "tools/call",
            name="session",
            arguments={"action": "restart"},
        )
        assert removed["error"] == {
            "code": -32602,
            "message": "tool not found",
        }, removed
        assert not (unused_workspace / ".agents").exists(), unused_workspace
        assert not list(unused_workspace.glob("sandbox-*")), unused_workspace
        transcript = client.finish()
        assert not (unused_workspace / ".agents").exists(), unused_workspace

        workspace = temporary / "send"
        workspace.mkdir()
        client = McpClient(
            binary,
            execution.serve("--worker", str(zod)),
            current_directory=workspace,
        )
        client.initialize_and_list_tools()
        assert not (workspace / ".agents/console/sessions").exists(), workspace
        client.send(r="echo echo")

        sessions = list((workspace / ".agents/console/sessions").iterdir())
        assert len(sessions) == 1, sessions
        assert not (workspace / ".mcp-console").exists(), workspace
        assert (sessions[0] / "transcript.md").is_file(), sessions
        assert (sessions[0] / "sessions/session-001.qmd").is_file(), sessions
        assert (sessions[0] / "capture/sessions/001").is_dir(), sessions
        assert (cell_output_path(sessions[0], 1)).read_text() == "zod: echo\n"
        events = live_recording_events(sessions[0])
        semantic_events = call_events(events)
        assert [event["event"] for event in semantic_events] == [
            "recording_started",
            "tool_call",
            "cell_output",
            "tool_result",
        ], events
        console_version = (
            subprocess.check_output([binary, "--version"], text=True)
            .strip()
            .removeprefix("mcp-console ")
        )
        assert semantic_events[0]["console_version"] == console_version
        markdown = (sessions[0] / "transcript.md").read_text(encoding="utf-8")
        assert f'"console_version": {json.dumps(console_version)}' in markdown
        assert semantic_events[1]["request"]["name"] == "send", semantic_events[1]
        client.finish()

        transcript.append(
            {
                "recording": {
                    "initialization and removed session tool only": "absent",
                    "materialized by": {
                        "send": [event["event"] for event in semantic_events]
                    },
                }
            }
        )
        return transcript


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_selects_existing_project_or_home_recording_directory(
    binary: Path, execution: Execution
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    records = []
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        for case in (
            "home",
            "project",
            "project path is a file",
            "console home",
            "project with console home",
        ):
            base = root / case
            home = base / "home"
            workspace = base / "workspace"
            home.mkdir(parents=True)
            workspace.mkdir()
            project_console = workspace / ".agents/console"
            in_project = case in ("project", "project with console home")
            if in_project:
                project_console.mkdir(parents=True)
            elif case == "project path is a file":
                project_console.parent.mkdir()
                project_console.write_text("occupied", encoding="utf-8")
            environment = os.environ | {"HOME": str(home)}
            environment.pop("MCP_CONSOLE_HOME", None)
            console_home = home / ".agents/console"
            if "console home" in case:
                console_home = base / "console"
                environment["MCP_CONSOLE_HOME"] = str(console_home)
            with McpClient(
                binary,
                execution.serve("--worker", str(zod)),
                environment=environment,
                current_directory=workspace,
                record_in_project=False,
                use_home_configuration=True,
            ) as client:
                client.initialize_and_list_tools()
                assert not (home / ".agents").exists()
                client.send(r="preview huge line")
                recording_root = project_console if in_project else console_home
                (session,) = (recording_root / "sessions").iterdir()
                log = session / "capture/sessions/001/cell-001.txt"
                assert log.is_file(), log
                public_path = (
                    Path(
                        f".agents/console/sessions/{session.name}/capture/sessions/001/cell-001.txt"
                    )
                    if in_project
                    else log
                )
                text = "".join(
                    block["text"]
                    for block in client.transcript[-1]["result"]["content"]
                    if block["type"] == "text"
                )
                assert str(public_path) in text
                if case == "home":
                    assert not (workspace / ".agents").exists()
                if in_project or "console home" in case:
                    assert not (home / ".agents").exists()
                client.finish()
            records.append(
                {
                    "case": case,
                    "recording_root": "project"
                    if in_project
                    else ("console home" if "console home" in case else "home"),
                }
            )
    return records


@requires(POSIX)
def test_rejects_non_utf8_home_recording_path(binary: Path) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        home = Path(os.fsdecode(os.fsencode(temporary) + b"/home-\xff"))
        workspace = root / "workspace"
        workspace.mkdir()
        project_console = workspace / ".agents/console"
        project_console.mkdir(parents=True)
        (project_console / "config.yaml").write_text("{}\n", encoding="utf-8")
        environment = os.environ | {"HOME": str(home)}
        environment.pop("MCP_CONSOLE_HOME", None)
        with McpClient(
            binary,
            ("serve", "--no-sandbox", "--worker", str(zod)),
            environment=environment,
            current_directory=workspace,
            record_in_project=False,
            use_home_configuration=True,
        ) as client:
            client.initialize_and_list_tools()
            # Configuration was captured at launch; recording is selected on send.
            (project_console / "config.yaml").unlink()
            project_console.rmdir()
            client.send(r="echo echo")
            assert client.transcript[-1]["result"] == {
                "content": [{"type": "text", "text": "zod: echo\n"}],
                "isError": False,
            }, client.transcript[-1]
            transcript, stderr = client.finish_with_standard_error()
        assert stderr == (
            "mcp-console: transcript recording disabled: recording path must be UTF-8\n"
        ), stderr
        assert not (workspace / ".agents/console/sessions").exists()
        transcript.append({"server stderr": stderr.strip()})
        return transcript


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_continues_without_record_when_record_cannot_be_created(
    binary: Path,
    execution: Execution,
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    with tempfile.TemporaryDirectory() as temporary_directory:
        workspace = Path(temporary_directory)
        (workspace / ".agents").mkdir()
        (workspace / ".agents/console").mkdir()
        (workspace / ".agents/console/sessions").write_text(
            "occupied", encoding="utf-8"
        )
        client = McpClient(
            binary,
            execution.serve("--worker", str(zod)),
            current_directory=workspace,
        )
        client.initialize_and_list_tools()

        client.send(r="echo echo")
        client.send(control="restart")

        client.request("tools/call", name="missing", arguments={})
        assert client.transcript[-1]["error"] == {
            "code": -32602,
            "message": "tool not found",
        }, client.transcript[-1]
        assert (workspace / ".agents/console/sessions").read_text(
            encoding="utf-8"
        ) == "occupied"
        transcript, standard_error = client.finish_with_standard_error()
        assert standard_error.count("\n") == 1, standard_error
        assert standard_error.startswith(
            "mcp-console: transcript recording disabled: failed to create "
        ), standard_error
        assert ".agents/console/sessions" in standard_error, standard_error
        transcript.append(
            {
                "server stderr": (
                    "mcp-console: transcript recording disabled: "
                    "<run record creation failed>"
                )
            }
        )
        return transcript


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_updates_quarto_without_rereading_journal(
    binary: Path, execution: Execution
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    with tempfile.TemporaryDirectory() as temporary_directory:
        workspace = Path(temporary_directory)
        client = McpClient(
            binary,
            execution.serve("--worker", str(zod)),
            current_directory=workspace,
        )
        finished = False
        journal_read_disabled = False
        try:
            client.initialize_and_list_tools()
            client.send(r="echo first")

            session = next((workspace / ".agents/console" / "sessions").iterdir())
            journal = session / "capture" / "events.jsonl"
            journal.chmod(0o200)
            journal_read_disabled = True
            # The harness cannot read this journal for a delivery receipt, so
            # wait for the projection itself.
            client.recording_receipts = False

            client.send(python="echo second")
            projection = session / "sessions/session-001.qmd"
            wait_for_checkpoint(
                lambda: (
                    projection
                    if "echo second" in projection.read_text(encoding="utf-8")
                    else None
                ),
                "second cell projected",
                root=projection.parent,
                client=client,
                timeout=client.response_timeout,
            )
            quarto = projection.read_text(encoding="utf-8")

            journal.chmod(0o600)
            journal_read_disabled = False
            assert "```{r}\n#| error: true\necho first\n```" in quarto, quarto
            assert "```{python}\n#| error: true\necho second\n```" in quarto, quarto

            transcript = client.finish()
            transcript.append(
                {
                    "quarto projection": {
                        "updated from incremental state": True,
                        "journal reopened for reading": False,
                    }
                }
            )
            finished = True
            return transcript
        finally:
            if journal_read_disabled:
                journal.chmod(0o600)
            if not finished:
                stop_client(client)


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_records_tool_calls_and_images(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    with tempfile.TemporaryDirectory() as temporary_directory, ExitStack() as resources:
        workspace = Path(temporary_directory)
        environment, _ = r_test_environment()
        environment["RETICULATE_PYTHON"] = ""
        record_resolved_r_library(environment, workspace)
        client = McpClient(
            binary,
            execution.serve("-c", "cache=host", "--worker", str(zod)),
            environment,
            current_directory=workspace,
            umask=0,
        )
        client.initialize_and_list_tools()
        client.send(
            r="emit image",
            stdin="recorded stdin\n",
            requirements={"r": ["praise"]},
        )
        session = next((workspace / ".agents/console" / "sessions").iterdir())
        quarto_path = session / "sessions/session-001.qmd"
        initial_quarto = quarto_path.read_text(encoding="utf-8")
        # Retain the old inode while checking atomic replacement. An unlinked
        # inode can otherwise be reused immediately on Linux filesystems.
        previous_quarto = resources.enter_context(quarto_path.open("rb"))
        quarto_before_inode = os.fstat(previous_quarto.fileno()).st_ino
        assert "    - praise" in initial_quarto
        assert "transcript-fixture" not in initial_quarto
        image_request_id = client.transcript[-1]["id"]
        invalid = client.request(
            "tools/call",
            name="send",
            arguments={"r": "1", "python": "1"},
            _meta={"progressToken": "record-me"},
        )
        quarto_before_python_requirement = quarto_path.read_text(encoding="utf-8")
        client.send(requirements={"python": ["transcript-fixture"]})
        preparation_request_id = client.transcript[-1]["id"]
        preparation_result = client.transcript[-1]["result"]
        assert preparation_result["isError"], preparation_result
        assert (
            quarto_path.read_text(encoding="utf-8") == quarto_before_python_requirement
        )
        accepted = client.send(requirements={"r": ["glue"]})
        assert not accepted.get("isError"), accepted
        accepted_request_id = client.transcript[-1]["id"]
        client.request("tools/call", name="missing", arguments={})

        sessions = list((workspace / ".agents/console" / "sessions").iterdir())
        assert len(sessions) == 1, sessions
        session = sessions[0]
        markdown_path = session / "transcript.md"
        for path in (session / "capture/events.jsonl", markdown_path, quarto_path):
            assert path.stat().st_mode & 0o777 == 0o600, path
        # Protocol capture continues after responses are sent. Finalize before
        # reading the complete journal instead of racing its next append.
        transcript = client.finish()
        journal_text = (session / "capture" / "events.jsonl").read_text(
            encoding="utf-8"
        )
        markdown_text = markdown_path.read_text(encoding="utf-8")
        quarto_text = quarto_path.read_text(encoding="utf-8")
        assert PNG_1X1 not in journal_text, journal_text
        assert PNG_1X1 not in markdown_text, markdown_text
        assert PNG_1X1 not in quarto_text, quarto_text
        events = [json.loads(line) for line in journal_text.splitlines()]
        semantic_events = call_events(events)
        assert [event["event"] for event in semantic_events] == [
            "recording_started",
            "tool_call",
            "image_retained",
            "cell_output",
            "tool_result",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
        ], events
        assert [event["sequence"] for event in events] == list(
            range(1, len(events) + 1)
        ), events
        assert any(event["event"] == "protocol" for event in events), events
        assert (
            len([event for event in events if event["event"] == "worker_started"]) == 1
        )
        assert [
            event["owner"]["interval"]
            for event in events
            if event["event"] == "output_text"
        ] == ["cell", "cell"]
        events = semantic_events
        run_id = events[0]["run_id"]
        assert run_id.endswith(f"-{client.process.pid:010d}"), run_id
        assert session.name == run_id, (session, run_id)
        assert Path(events[0]["working_directory"]).samefile(workspace), events[0]
        console_version = (
            subprocess.check_output([binary, "--version"], text=True)
            .strip()
            .removeprefix("mcp-console ")
        )
        assert events[0]["console_version"] == console_version
        assert all(event["run_id"] == run_id for event in events), events
        assert all("schema_version" not in event for event in events), events
        assert events[1]["call_id"] == events[2]["call_id"] == 1, events
        assert events[3]["call_id"] == events[2]["call_id"], events
        assert events[1]["request_id"] == image_request_id, events[1]
        assert events[1]["request"] == {
            "name": "send",
            "arguments": {
                "r": "emit image",
                "stdin": "recorded stdin\n",
                "requirements": {"r": ["praise"]},
            },
        }, events[1]
        assert {
            key: events[2][key]
            for key in ("artifact_id", "call_id", "path", "mime_type", "bytes")
        } == {
            "artifact_id": 1,
            "call_id": 1,
            "path": "capture/sessions/001/cell-001-001.png",
            "mime_type": "image/png",
            "bytes": len(base64.b64decode(PNG_1X1)),
        }, events[2]
        assert {
            key: events[3][key]
            for key in (
                "path",
                "retained_bytes",
                "inline_omitted_bytes",
                "discarded_bytes",
                "retention_limit_bytes",
            )
        } == {
            "path": "capture/sessions/001/cell-001.txt",
            "retained_bytes": len("before image\nafter image\n"),
            "inline_omitted_bytes": 0,
            "discarded_bytes": 0,
            "retention_limit_bytes": CELL_OUTPUT_RETENTION_LIMIT,
        }, events[3]
        assert events[4]["result"] == {
            "content": [
                {"type": "text", "text": "before image\n"},
                {
                    "type": "image",
                    "artifactId": 1,
                    "path": "capture/sessions/001/cell-001-001.png",
                    "mimeType": "image/png",
                },
                {"type": "text", "text": "after image\n"},
            ],
            "isError": False,
        }, events[4]
        assert events[5]["call_id"] == events[6]["call_id"] == 2, events
        assert events[5]["request_id"] == invalid["id"], events[5]
        assert events[5]["request"] == {
            "name": "send",
            "arguments": {"r": "1", "python": "1"},
            "_meta": {"progressToken": "record-me"},
        }, events[5]
        assert events[6]["result"] == {
            "content": [
                {
                    "type": "text",
                    "text": "only one of `r`, `python`, or `sql` may be supplied",
                }
            ],
            "isError": True,
        }, events[6]
        assert events[7]["call_id"] == events[8]["call_id"] == 3, events
        assert events[7]["request_id"] == preparation_request_id, events[7]
        assert events[7]["request"] == {
            "name": "send",
            "arguments": {
                "requirements": {"python": ["transcript-fixture"]},
            },
        }, events[7]
        assert events[8]["result"] == preparation_result, events[8]
        assert events[9]["call_id"] == events[10]["call_id"] == 4, events
        assert events[9]["request_id"] == accepted_request_id, events[9]
        assert events[9]["request"] == {
            "name": "send",
            "arguments": {"requirements": {"r": ["glue"]}},
        }, events[9]
        assert events[10]["result"] == accepted, events[10]
        assert [event["request"]["name"] for event in events if "request" in event] == [
            "send",
            "send",
            "send",
            "send",
        ], events
        assert all(
            event.get("request", {}).get("name") != "missing" for event in events
        ), events

        image_path = session / events[4]["result"]["content"][1]["path"]
        output_path = session / events[3]["path"]
        image_bytes = image_path.read_bytes()
        assert image_bytes == base64.b64decode(PNG_1X1), image_path
        assert output_path.read_text(encoding="utf-8") == "before image\nafter image\n"
        directory_modes = {
            path.relative_to(workspace).as_posix(): path.stat().st_mode & 0o777
            for path in (
                workspace / ".agents",
                workspace / ".agents/console",
                workspace / ".agents/console" / "sessions",
                session,
                session / "capture",
                session / "capture/sessions",
                session / "capture/sessions/001",
                session / "sessions",
            )
        }
        assert set(directory_modes.values()) == {0o700}, directory_modes
        file_modes = {
            path.relative_to(workspace).as_posix(): path.stat().st_mode & 0o777
            for path in (
                session / "capture" / "events.jsonl",
                markdown_path,
                quarto_path,
                image_path,
                output_path,
            )
        }
        assert image_path.stat().st_mode & 0o777 == 0o400
        assert output_path.stat().st_mode & 0o777 == 0o400
        assert set(file_modes.values()) == {0o400}, file_modes

        for event in events:
            assert event["at"].endswith("Z"), event
            datetime.fromisoformat(event["at"])
            event["at"] = "<UTC timestamp>"
            event["run_id"] = "<run ID>"
            # Full capture sequence continuity was checked above. Protocol and
            # asynchronous discovery can change the gaps between this subset.
            event["sequence"] = "<capture sequence>"
            if "request_id" in event:
                event["request_id"] = "<request ID>"
        events[0]["working_directory"] = "<workspace>"
        events[0]["console_version"] = "<Console version>"
        assert journal_text.endswith("\n"), journal_text
        assert markdown_text.endswith("\n"), markdown_text
        assert quarto_text.endswith("\n"), quarto_text
        assert "```{r}\n#| error: true\nemit image\n```" in quarto_text
        assert quarto_path.stat().st_ino != quarto_before_inode
        assert "    - praise" in quarto_text
        assert "    - glue" in quarto_text
        assert "transcript-fixture" not in quarto_text
        assert "  python-version: []" in quarto_text
        assert all(
            excluded not in quarto_text
            for excluded in (
                "recorded stdin",
                "before image",
                "Image 1",
                "Result for call",
            )
        ), quarto_text

        return TranscriptWithCompanions(
            transcript=transcript,
            companions={
                "events.yaml": [
                    events,
                    {
                        "produced session": {
                            "root": ".agents/console/sessions/<run ID>",
                            "files": [
                                "capture/events.jsonl",
                                "transcript.md",
                                "sessions/session-001.qmd",
                                "capture/sessions/001/cell-001-001.png",
                                "capture/sessions/001/cell-001.txt",
                            ],
                        }
                    },
                ],
            },
        )


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_disables_recording_after_transcript_failure(
    binary: Path, execution: Execution
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    with tempfile.TemporaryDirectory() as temporary_directory:
        workspace = Path(temporary_directory)
        client = McpClient(
            binary,
            execution.serve("--worker", str(zod)),
            current_directory=workspace,
        )
        client.initialize_and_list_tools()
        client.send(r="echo echo")
        session = next((workspace / ".agents/console" / "sessions").iterdir())
        (session / "capture/sessions/001/cell-002-001.png").mkdir()

        client.send(r="emit image")
        image_result = client.transcript[-1]["result"]
        assert image_result == {
            "content": [
                {"type": "text", "text": "before image\n"},
                {"type": "image", "data": PNG_1X1, "mimeType": "image/png"},
                {"type": "text", "text": "after image\n"},
            ],
            "isError": False,
        }, image_result

        journal = session / "capture" / "events.jsonl"
        journal_after_failure = journal.read_text(encoding="utf-8")
        events = [json.loads(line) for line in journal_after_failure.splitlines()]
        semantic_events = call_events(events)
        assert [event["event"] for event in semantic_events] == [
            "recording_started",
            "tool_call",
            "cell_output",
            "tool_result",
            "tool_call",
        ], events
        assert journal_after_failure.endswith("\n"), journal_after_failure

        client.send(r="echo echo")
        assert journal.read_text(encoding="utf-8") == journal_after_failure

        transcript, standard_error = client.finish_with_standard_error()
        assert journal.read_text(encoding="utf-8") == journal_after_failure
        for path in session.rglob("*"):
            if path.is_file():
                assert path.stat().st_mode & 0o222 == 0, path
        assert standard_error.count("\n") == 1, standard_error
        assert standard_error.startswith(
            "mcp-console: transcript recording disabled: failed to create "
        ), standard_error
        assert "/capture/sessions/001/cell-002-001.png" in standard_error, (
            standard_error
        )
        transcript.append(
            {
                "journal after failure": [event["event"] for event in semantic_events],
                "complete final line": True,
                "post-failure append": False,
                "server stderr": (
                    "mcp-console: transcript recording disabled: "
                    "<artifact persistence failed>"
                ),
            }
        )
        return transcript


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_keeps_recording_after_cell_output_failure(
    binary: Path, execution: Execution
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    with tempfile.TemporaryDirectory() as temporary_directory:
        workspace = Path(temporary_directory)
        client = McpClient(
            binary,
            execution.serve("--worker", str(zod)),
            current_directory=workspace,
        )
        client.initialize_and_list_tools()
        client.send(r="echo first")

        session = next((workspace / ".agents/console" / "sessions").iterdir())
        outputs = session / "capture/sessions/001"
        collision = outputs / "cell-002.txt"
        collision.mkdir()

        client.send(r="echo second")
        failure = last_tool_text(client)
        public_output = (
            f".agents/console/sessions/{session.name}/capture/sessions/001/cell-002.txt"
        )
        assert "cell output file was not created" in failure, failure
        assert public_output in failure, failure
        assert str(workspace) not in failure, failure
        assert (
            "text omitted from inline responses will be permanently discarded"
            in failure
        )
        assert failure.endswith("zod: second\n"), failure
        client.transcript[-1]["result"]["content"][0]["text"] = (
            "[cell output file was not created: <output file creation failed>; "
            "text omitted from inline responses will be permanently discarded]\n"
            "zod: second\n"
        )

        collision.rmdir()
        client.send(r="echo third")
        assert last_tool_text(client) == "zod: third\n"
        assert (outputs / "cell-003.txt").read_text(encoding="utf-8") == (
            "zod: third\n"
        )

        events = live_recording_events(session)
        semantic_events = call_events(events)
        assert [event["event"] for event in semantic_events] == [
            "recording_started",
            "tool_call",
            "cell_output",
            "tool_result",
            "tool_call",
            "tool_result",
            "tool_call",
            "cell_output",
            "tool_result",
        ], events
        assert [
            event["call_id"] for event in events if event["event"] == "cell_output"
        ] == [
            1,
            3,
        ], events
        return client.finish()


@executions(DIRECT, SANDBOXED)
@requires(PROCESS_EVENTS)
def test_flushes_calls_and_keeps_unpolled_images(
    binary: Path, execution: Execution
) -> Transcript:
    zod = Path(__file__).resolve().parents[3] / "fixtures" / "zod"
    with tempfile.TemporaryDirectory() as temporary_directory:
        temporary = Path(temporary_directory)
        workspace = temporary / "workspace"
        workspace.mkdir()
        environment = os.environ.copy()
        environment["TMPDIR"] = temporary_directory
        client = McpClient(
            binary,
            execution.serve("--worker", str(zod)),
            environment,
            current_directory=workspace,
        )
        client.initialize_and_list_tools()

        waiting = client.start_send(r="complete after release")
        started = wait_for_marker(
            temporary,
            "zod-evaluation-started",
            client,
        )
        session = next((workspace / ".agents/console" / "sessions").iterdir())
        journal = session / "capture" / "events.jsonl"

        def live_events() -> list[dict]:
            # Protocol capture can append after a response arrives. Decode only
            # newline-terminated records, including when the tail splits UTF-8.
            return [json.loads(line) for line in journal.read_bytes().split(b"\n")[:-1]]

        markdown = session / "transcript.md"
        quarto = session / "sessions/session-001.qmd"
        before_release = live_events()
        assert [event["event"] for event in call_events(before_release)] == [
            "recording_started",
            "tool_call",
        ], before_release
        before_release_markdown = markdown.read_text(encoding="utf-8")
        before_release_quarto = quarto.read_text(encoding="utf-8")
        assert "## Call 1: R" in before_release_markdown
        assert "complete after release" in before_release_markdown
        assert "## Result for call 1" not in before_release_markdown
        assert (
            "```{r}\n#| error: true\ncomplete after release\n```"
            in before_release_quarto
        )
        markdown_inode = markdown.stat().st_ino
        quarto_inode = quarto.stat().st_ino

        (started.parent / "zod-release-evaluation").touch()
        client.receive(waiting)
        after_release = live_events()
        assert [event["event"] for event in call_events(after_release)] == [
            "recording_started",
            "tool_call",
            "cell_output",
            "tool_result",
        ], after_release
        after_release_markdown = markdown.read_text(encoding="utf-8")
        after_release_quarto = quarto.read_text(encoding="utf-8")
        assert after_release_markdown.startswith(before_release_markdown)
        assert markdown.stat().st_ino == markdown_inode
        assert "## Result for call 1" in after_release_markdown
        assert "zod: complete after release" in after_release_markdown
        assert after_release_quarto == before_release_quarto
        assert quarto.stat().st_ino == quarto_inode

        client.send(
            r="emit image before completion",
            timeout_ms=0,
        )
        assert without_elapsed_result(client.transcript[-1]["result"]) == {
            "content": [
                {"type": "text", "text": "\n[running; poll with an empty send]"}
            ],
            "isError": False,
        }, client.transcript[-1]
        block = client.transcript[-1]["result"]["content"][0]
        block["text"] = normalize_elapsed(block["text"]).replace(
            "\n", "<leading newline>", 1
        )
        image_started = wait_for_marker(
            temporary,
            "zod-image-evaluation-started",
            client,
        )
        (image_started.parent / "zod-release-image").touch()
        wait_for_marker(temporary, "zod-image-processed", client)

        final_events = live_events()
        assert [event["event"] for event in call_events(final_events)] == [
            "recording_started",
            "tool_call",
            "cell_output",
            "tool_result",
            "tool_call",
            "tool_result",
            "image_retained",
        ], final_events
        (artifact,) = [
            event for event in final_events if event["event"] == "image_retained"
        ]
        assert {
            key: artifact[key]
            for key in ("artifact_id", "call_id", "path", "mime_type", "bytes")
        } == {
            "artifact_id": 1,
            "call_id": 2,
            "path": "capture/sessions/001/cell-002-001.png",
            "mime_type": "image/png",
            "bytes": len(base64.b64decode(PNG_1X1)),
        }, artifact
        image_path = session / artifact["path"]
        assert image_path.read_bytes() == base64.b64decode(PNG_1X1), image_path
        unpolled_markdown = markdown.read_text(encoding="utf-8")
        unpolled_quarto = quarto.read_text(encoding="utf-8")
        assert unpolled_markdown.startswith(after_release_markdown)
        assert markdown.stat().st_ino == markdown_inode
        assert f"[Image {artifact['artifact_id']} from call 2]" in unpolled_markdown
        assert artifact["path"] in unpolled_markdown
        assert unpolled_quarto.startswith(after_release_quarto)
        assert (
            "```{r}\n#| error: true\nemit image before completion\n```"
            in unpolled_quarto
        )
        assert quarto.stat().st_ino != quarto_inode
        unpolled_quarto_inode = quarto.stat().st_ino

        (image_started.parent / "zod-release-image-completion").touch()

        def completed_image_output() -> Path | None:
            # Completion belongs to call 2 even when it precedes the next poll.
            if any(
                event.get("event") == "cell_output" and event.get("call_id") == 2
                for event in live_events()
            ):
                return journal
            return None

        wait_for_checkpoint(
            completed_image_output,
            "unpolled cell completion recorded",
            root=journal,
            client=client,
        )
        completed_events = live_events()
        completed_semantic = call_events(completed_events)
        assert [event["event"] for event in completed_semantic] == [
            *[event["event"] for event in call_events(final_events)],
            "cell_output",
        ], completed_events
        assert completed_semantic[-1]["call_id"] == 2, completed_semantic[-1]
        client.send(session="default", timeout_ms=3_000)
        poll_result = client.transcript[-1]["result"]
        assert poll_result == {
            "content": [{"type": "image", "data": PNG_1X1, "mimeType": "image/png"}],
            "isError": False,
        }, poll_result
        polled_events = live_events()
        polled_semantic = call_events(polled_events)
        assert [event["event"] for event in polled_semantic] == [
            *[event["event"] for event in completed_semantic],
            "tool_call",
            "tool_result",
        ], polled_events
        assert polled_semantic[-1]["call_id"] == 3, polled_semantic[-1]
        assert polled_semantic[-1]["result"] == {
            "content": [
                {
                    "type": "image",
                    "mimeType": "image/png",
                    "artifactId": artifact["artifact_id"],
                    "path": artifact["path"],
                }
            ],
            "isError": False,
        }, polled_semantic[-1]
        polled_markdown = markdown.read_text(encoding="utf-8")
        polled_quarto = quarto.read_text(encoding="utf-8")
        assert polled_markdown.startswith(unpolled_markdown)
        assert markdown.stat().st_ino == markdown_inode
        assert "## Call 3: Poll" in polled_markdown
        assert "## Result for call 3" in polled_markdown
        assert polled_quarto == unpolled_quarto
        assert quarto.stat().st_ino == unpolled_quarto_inode

        transcript = client.finish()
        finalized_events = [
            json.loads(line) for line in journal.read_bytes().splitlines()
        ]
        assert finalized_events[-1]["event"] == "recording_ended", finalized_events[-1]
        transcript.append(
            {
                "live journal": {
                    "while first call was running": [
                        event["event"] for event in call_events(before_release)
                    ],
                    "after first call completed": [
                        event["event"] for event in call_events(after_release)
                    ],
                    "unpolled image": {
                        "event": artifact["event"],
                        "path": artifact["path"],
                        "data": "<byte-identical decoded PNG>",
                    },
                    "later poll result": polled_semantic[-1]["result"],
                    "Markdown projection": {
                        "live before result": True,
                        "each snapshot retained as an exact prefix": True,
                        "inode retained": True,
                    },
                    "Quarto projection": "source cells only",
                }
            }
        )
        return transcript


def configure_r_startup(
    root: Path, environment: dict[str, str], rscript: Path, source: str
) -> None:
    libraries = subprocess.check_output(
        [
            shutil.which("ir"),
            "run",
            "--rscript",
            str(rscript),
            "--vanilla",
            "--isolated",
            "--with",
            "DBI",
            "--with",
            "duckdb",
            "--with",
            "reticulate",
            "-e",
            "writeLines(.libPaths())",
        ],
        env=environment,
        cwd=root,
        text=True,
    ).splitlines()
    environment["R_LIBS"] = os.pathsep.join(libraries)
    # Configured Console startup runs after the runtime and plot device attach.
    # Its existing contract includes selecting the SQL connection.
    # fmt: r
    connection = code("""
        .console$sql_connection(DBI::dbConnect(duckdb::duckdb()))
        """)
    configuration = root / ".agents/console/config.yaml"
    configuration.parent.mkdir(parents=True)
    configuration.write_text(
        format_yaml({"startup": {"language": "r", "code": connection + source}})
    )


@contextmanager
def local_discovery(root: Path, *, image: bool = False, fail: bool = False):
    environment, rscript = r_test_environment()
    home = Path(environment["R_HOME"])
    with ExitStack() as resources:
        if image:
            configure_r_startup(root, environment, rscript, STARTUP_PLOT)
        probe_environment, discovery = resources.enter_context(
            discovery_environment(r_home=None if fail else home)
        )
        environment.pop("R_HOME", None)
        # Keep the gated R probe first without hiding host dependency tools.
        environment["PATH"] = os.pathsep.join(
            (probe_environment["PATH"], environment["PATH"])
        )
        environment["RETICULATE_PYTHON"] = sys.executable
        environment[LOADER_VARIABLE] = str(
            build_interposer(root, "discovery_diagnostic")
        )
        yield environment, discovery


@requires(NATIVE_FIXTURES, PROCESS_EVENTS, R)
@requires(command("rustc"))
def test_records_early_calls_before_discovery(binary: Path) -> Transcript:
    with tempfile.TemporaryDirectory() as directory, ExitStack() as resources:
        root = Path(directory)
        environment, discovery = resources.enter_context(local_discovery(root))
        try:
            with McpClient(
                binary,
                DIRECT.serve(),
                environment,
                root,
            ) as client:
                client.initialize_and_list_tools()
                discovery.wait("discovery diagnostic emitted")

                sessions = root / ".agents/console/sessions"
                assert not sessions.exists(), sessions
                client.expect(
                    "\n[phase: startup]\n[worker starting]",
                    requirements={"action": "get"},
                    timeout_ms=0,
                )
                discovery.release()
                client.send(requirements={"action": "get"})
                client.send(r="42L")
                client.finish()
            (session,) = (root / ".agents/console/sessions").iterdir()
            events = [
                json.loads(line)
                for line in (session / "capture/events.jsonl").read_text().splitlines()
            ]
            early = [
                event
                for event in events
                if event.get("call_id") == 1
                and event["event"] in {"tool_call", "tool_result"}
            ]
            (discovered,) = [
                event for event in events if event["event"] == "environment_discovered"
            ]
            assert events[0]["startup_requirements"] is None, events[0]
            assert discovered["startup_requirements"]["python"] == [], discovered
            assert "schema_version" not in discovered, discovered
            assert [event["event"] for event in early] == [
                "tool_call",
                "tool_result",
            ], early
            assert all(event["sequence"] < discovered["sequence"] for event in early), (
                events
            )
            assert all(event["at"] < discovered["at"] for event in early), events
            markdown = (session / "transcript.md").read_text()
            assert markdown.index("## Call 1:") < markdown.index("## Call 2:"), markdown
            assert "## Runtime discovery" not in markdown, markdown
            quarto = (session / "sessions/session-001.qmd").read_text()
            assert "environment: unknown" not in quarto, quarto
            assert "eval: false" not in quarto, quarto
            return [{"early_call_and_result_precede_discovery": True}]
        finally:
            discovery.release()


@requires(NATIVE_FIXTURES, PROCESS_EVENTS, R, SQL, command("ir"))
@requires(command("rustc"))
def test_records_early_calls_before_startup_artifacts(binary: Path) -> Transcript:
    reference_environment, rscript = r_test_environment()
    (expected_image,) = reference_plots(
        rscript,
        reference_environment,
        STARTUP_PLOT,
        width=4,
        height=3,
        dpi=100,
        pages=1,
    )
    with tempfile.TemporaryDirectory() as directory, ExitStack() as resources:
        root = Path(directory)
        environment, discovery = resources.enter_context(
            local_discovery(root, image=True)
        )
        try:
            with McpClient(
                binary,
                DIRECT.serve(),
                environment,
                root,
            ) as client:
                client.initialize_and_list_tools()
                discovery.wait("discovery awaiting release")

                client.expect(
                    "\n[phase: startup]\n[worker starting]",
                    requirements={"action": "get"},
                    timeout_ms=0,
                )
                sessions = root / ".agents/console/sessions"
                assert not list(sessions.glob("*/capture/sessions/*/*.png"))
                discovery.release()
                # Discovery release still leaves host dependency preparation
                # before startup can render its image.
                image = wait_for_checkpoint(
                    lambda: next(sessions.glob("*/capture/sessions/*/*.png"), None),
                    "startup image retained after discovery",
                    root=sessions,
                    recursive=True,
                    client=client,
                    timeout=client.response_timeout,
                )
                client.finish()
            session = image.parents[3]
            events = [
                json.loads(line)
                for line in (session / "capture/events.jsonl").read_text().splitlines()
            ]
            (artifact,) = [
                event for event in events if event["event"] == "image_retained"
            ]
            early = [
                event
                for event in events
                if event.get("call_id") == 1
                and event["event"] in {"tool_call", "tool_result"}
            ]
            assert [event["event"] for event in early] == [
                "tool_call",
                "tool_result",
            ], events
            assert all(event["sequence"] < artifact["sequence"] for event in early), (
                events
            )
            assert all(event["at"] < artifact["at"] for event in early), events
            assert events[0]["event"] == "recording_started", events
            assert all(events[0]["at"] <= event["at"] for event in events), events
            assert artifact["call_id"] is None, artifact
            assert image.read_bytes() == expected_image
            markdown = (session / "transcript.md").read_text()
            assert markdown.index("## Call 1:") < markdown.index(
                "## Image 1 for session"
            ), markdown
            return [{"early_call_and_result_precede_startup_artifact": True}]
        finally:
            discovery.release()


@requires(NATIVE_FIXTURES, R)
@requires(command("rustc"))
def test_records_early_calls_when_discovery_fails(binary: Path) -> Transcript:
    with tempfile.TemporaryDirectory() as directory, ExitStack() as resources:
        root = Path(directory)
        environment, discovery = resources.enter_context(
            local_discovery(root, fail=True)
        )
        try:
            with McpClient(
                binary,
                DIRECT.serve(),
                environment,
                root,
            ) as client:
                client.initialize_and_list_tools()
                discovery.wait("discovery awaiting failure release")

                client.expect(
                    "\n[phase: startup]\n[worker starting]",
                    requirements={"action": "get"},
                    timeout_ms=0,
                )
                client.send(r="stop('failed discovery ran the cell')", timeout_ms=0)
                discovery.release()
                response = client.send(session="default")
                assert response["isError"], response
                assert "fixture R discovery failed" in last_result_text(client)
                _, stderr = client.finish_with_standard_error(expected_exit_status=1)
                assert "fixture R discovery failed" in stderr, stderr
            (session,) = (root / ".agents/console/sessions").iterdir()
            events = [
                json.loads(line)
                for line in (session / "capture/events.jsonl").read_text().splitlines()
            ]
            calls = [event for event in events if event["event"] == "tool_call"]
            results = [event for event in events if event["event"] == "tool_result"]
            assert events[0]["event"] == "recording_started", events
            assert all(events[0]["at"] <= event["at"] for event in events), events
            assert len(calls) == len(results) == 3, events
            assert [event["call_id"] for event in calls] == [1, 2, 3], calls
            assert [event["call_id"] for event in results] == [1, 2, 3], results
            assert results[-1]["result"]["isError"], results[-1]
            assert "fixture R discovery failed" in str(results[-1]), results[-1]
            assert sum(event["event"] == "startup_failed" for event in events) == 1
            raw = b"".join(
                base64.b64decode(event["data"])
                for event in events
                if event["event"] == "process_output"
            )
            assert raw == b"preparation detail\n"
            assert all(
                event["discarded_bytes"] == 0
                for event in events
                if event["event"] == "process_output"
            )
            assert not any(event["event"] == "worker_started" for event in events)
            assert not list((session / "sessions").glob("*.qmd"))
            assert (
                "stop('failed discovery ran the cell')"
                in (session / "transcript.md").read_text()
            )
            assert events[0]["dynamic_resolution"] is None, events[0]
            assert events[0]["python_preparation"] is None, events[0]
            assert events[0]["startup_requirements"] is None, events[0]
            return [{"early_calls_recorded": 3, "startup_failure_recorded": True}]
        finally:
            discovery.release()


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_does_not_attempt_recording_without_a_tool_call(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        reached = FifoCheckpoint.create(root / "startup-output")
        release = FifoCheckpoint.create(root / "startup-release")
        # fmt: r
        source = code(r"""
            cat("unrecorded startup text\n")
            ready <- fifo(
              Sys.getenv("MCP_CONSOLE_TEST_STARTUP_OUTPUT"),
              "wb",
              blocking = TRUE
            )
            writeBin(charToRaw("1"), ready)
            close(ready)
            gate <- fifo(
              Sys.getenv("MCP_CONSOLE_TEST_STARTUP_RELEASE"),
              "rb",
              blocking = TRUE
            )
            readBin(gate, "raw", 1L)
            """)
        try:
            (root / ".agents/console").mkdir(parents=True)
            (root / ".agents/console/sessions").write_text("occupied")
            with startup_r_package(root, source) as env:
                env.update(
                    RETICULATE_PYTHON=sys.executable,
                    MCP_CONSOLE_TEST_STARTUP_OUTPUT=str(reached.path),
                    MCP_CONSOLE_TEST_STARTUP_RELEASE=str(release.path),
                )
                args = (
                    execution.serve("--writable-root", str(root))
                    if execution == SANDBOXED
                    else execution.serve()
                )
                with McpClient(binary, args, env, root) as client:
                    client.initialize_and_list_tools()
                    reached.wait("startup text emitted", timeout=60)
                    client.request("ping")
                    _, stderr = client.finish_with_standard_error()
                    assert stderr == "", stderr
                    assert not any(
                        entry.get("method") == "tools/call"
                        for entry in client.transcript
                    ), client.transcript
                assert (root / ".agents/console/sessions").read_text() == "occupied"
            return [{"recording_attempted_without_send": False}]
        finally:
            reached.close()
            release.close()


@requires(POSIX, R, SQL, command("ir"))
@executions(DIRECT, SANDBOXED)
def test_discards_startup_capture_without_a_tool_call(
    binary: Path, execution: Execution
) -> Transcript:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        reached = FifoCheckpoint.create(root / "startup-output")
        release = FifoCheckpoint.create(root / "startup-release")
        # fmt: r
        source = code(r"""
            cat(paste0(rep("startup text\n", 20000L), collapse = ""))
            graphics::plot(1:3)
            graphics::plot(3:1)
            grDevices::dev.off()
            ready <- fifo(
              Sys.getenv("MCP_CONSOLE_TEST_STARTUP_OUTPUT"),
              "wb",
              blocking = TRUE
            )
            writeBin(charToRaw("1"), ready)
            close(ready)
            gate <- fifo(
              Sys.getenv("MCP_CONSOLE_TEST_STARTUP_RELEASE"),
              "rb",
              blocking = TRUE
            )
            readBin(gate, "raw", 1L)
            """)
        env, rscript = r_test_environment()
        configure_r_startup(root, env, rscript, source)
        env.update(
            RETICULATE_PYTHON=sys.executable,
            MCP_CONSOLE_TEST_STARTUP_OUTPUT=str(reached.path),
            MCP_CONSOLE_TEST_STARTUP_RELEASE=str(release.path),
        )
        try:
            args = (
                execution.serve("--writable-root", str(root))
                if execution == SANDBOXED
                else execution.serve()
            )
            with McpClient(binary, args, env, root) as client:
                client.initialize_and_list_tools()
                reached.wait("startup output drained without a tool call", timeout=60)
                client.request("ping")
                client.stdin.close()
                assert client.process.wait(timeout=12) == 0
                assert client.stdout.read() == ""
                assert client.stderr.read() == ""
            assert not (root / ".agents/console/sessions").exists()
            return [{"startup_capture_discarded_without_send": True}]
        finally:
            reached.close()
            release.close()


@requires(NATIVE_FIXTURES, PROCESS_EVENTS, R, command("ir"))
@requires(command("rustc"))
def test_preserves_early_requirements_in_published_quarto(binary: Path) -> Transcript:
    with tempfile.TemporaryDirectory() as directory, ExitStack() as resources:
        root = Path(directory)
        environment, discovery = resources.enter_context(local_discovery(root))
        try:
            with McpClient(binary, DIRECT.serve(), environment, root) as client:
                client.initialize_and_list_tools()
                discovery.wait("discovery is held before recording metadata is known")

                admitted = client.send(
                    r="42L", requirements={"r": ["praise"]}, timeout_ms=0
                )
                assert not admitted["isError"], admitted
                assert last_tool_text(client).endswith(
                    "[running; poll with an empty send]"
                ), admitted
                discovery.release()
                result = client.send(session="default")
                assert not result["isError"], result
                assert last_tool_text(client).endswith("[1] 42\n")
                transcript = client.finish()
            (recording,) = (root / ".agents/console/sessions").iterdir()
            quarto = (recording / "sessions/session-001.qmd").read_text()
            assert "    - praise\n" in quarto, quarto
            assert "```{r}\n#| error: true\n42L\n```" in quarto, quarto
            assert "eval: false" not in quarto, quarto
            return transcript
        finally:
            discovery.release()


@requires(NATIVE_FIXTURES, R)
@requires(command("rustc"))
def test_discards_startup_failure_without_send(binary: Path) -> Transcript:
    with tempfile.TemporaryDirectory() as directory, ExitStack() as resources:
        root = Path(directory)
        environment, discovery = resources.enter_context(
            local_discovery(root, fail=True)
        )
        recorded = resources.enter_context(
            closing(FifoCheckpoint.create(root / "failure-recorded"))
        )
        environment["MCP_CONSOLE_TEST_STARTUP_FAILURE_RECORDED"] = str(recorded.path)
        try:
            with McpClient(binary, DIRECT.serve(), environment, root) as client:
                client.initialize_and_list_tools()
                discovery.wait("discovery ready to fail without send")

                client.request("ping")
                discovery.release()
                # The discovery process has emitted its failure before shutdown;
                # no send is used to observe or activate the recording.
                recorded.wait(
                    "server recorded the completed startup failure", client=client
                )
                _, stderr = client.finish_with_standard_error(expected_exit_status=1)
                assert "fixture R discovery failed" in stderr
                assert not (root / ".agents/console/sessions").exists()
                assert not any(
                    entry.get("method") == "tools/call" for entry in client.transcript
                )
            return [{"startup_failure_without_send": "capture discarded"}]
        finally:
            discovery.release()


if __name__ == "__main__":
    run_this_suite(__file__)
