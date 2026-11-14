#!/usr/bin/env -S uv run --script

import json
import base64
import os
import sys
import tempfile
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.recording.test_process import recording_events
from support.checkpoints import FifoCheckpoint, wait_for_checkpoint
from support.client import McpClient
from support.execution import DIRECT
from support.native import LOADER_VARIABLE, build_interposer
from support.normalization import code
from support.r import startup_r_package
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, POSIX, R, requires
from support.suites import run_this_suite


@requires(NATIVE_FIXTURES, POSIX)
def test_eof_publishes_unpolled_output_receipts(binary: Path) -> Transcript:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        with (
            closing(FifoCheckpoint.create(root / "cell-release")) as release,
            closing(FifoCheckpoint.create(root / "output-retained")) as retained,
        ):
            environment = {
                **os.environ,
                LOADER_VARIABLE: str(build_interposer(root, "recording_observation")),
                "MCP_CONSOLE_TEST_OBSERVED_TEXT": "unpolled retained text\n",
                "MCP_CONSOLE_TEST_JOURNAL_OBSERVED": str(retained.path),
                "MCP_CONSOLE_TEST_ZOD_PYTHON_CELLS": str(root / "cells.jsonl"),
            }
            with McpClient(
                binary,
                DIRECT.serve(
                    "--worker",
                    str(Path(__file__).resolve().parents[3] / "fixtures/zod"),
                ),
                environment,
                root,
            ) as client:
                client.initialize_and_list_tools()
                try:
                    response = client.send(
                        # fmt: python
                        python=code(f"""
                            with open({str(release.path)!r}, "rb", buffering=0) as gate:
                                assert gate.read(1) == b"1"
                            print("unpolled retained text")
                            """),
                        timeout_ms=0,
                    )
                    assert not response["isError"], response
                    release.release()
                    retained.wait("unpolled cell text is retained", client=client)
                    transcript = client.finish()
                finally:
                    release.release()
            (recording,) = (root / ".agents/console/sessions").iterdir()
            assert (
                recording / "capture/sessions/001/cell-001.txt"
            ).read_text() == "unpolled retained text\n"
            events = recording_events(recording)
            output = [e for e in events if e["event"] == "cell_output"]
            assert len(output) == 1, events
            assert output[0]["retained_bytes"] == len("unpolled retained text\n")
            assert output[0]["call_id"] == 1
            assert events[-1]["event"] == "recording_ended"
            return [
                *transcript[:3],
                {"capture": "unpolled output receipt precedes recording end"},
            ]


@requires(NATIVE_FIXTURES, POSIX, R)
def test_failed_publication_invalidates_startup_output_receipts(
    binary: Path,
) -> Transcript:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        with closing(FifoCheckpoint.create(root / "output-retained")) as retained:
            # fmt: r
            startup = code("""
                cat("receipt startup\\n", strrep("s", 32768L), "\\nreceipt tail\\n", sep = "")
                """)
            with startup_r_package(root, startup) as environment:
                environment.update(
                    {
                        "RETICULATE_PYTHON": sys.executable,
                        LOADER_VARIABLE: str(
                            build_interposer(root, "recording_observation")
                        ),
                        "MCP_CONSOLE_TEST_OBSERVED_TEXT": "\nreceipt tail\n",
                        "MCP_CONSOLE_TEST_JOURNAL_OBSERVED": str(retained.path),
                    }
                )
                with McpClient(binary, DIRECT.serve(), environment, root) as client:
                    client.initialize_and_list_tools()
                    retained.wait(
                        "startup raw output retained before activation",
                        timeout=600,
                        client=client,
                    )
                    (root / ".agents/console/sessions").write_text("not a directory")
                    result = client.send(session="default")
                    text = "".join(
                        c["text"] for c in result["content"] if c["type"] == "text"
                    )
                    assert "omitted text unavailable" in text, text
                    assert "raw log:" not in text, text
                    assert not result["isError"], result
                    client.expect("42\n", python="21 * 2")
                    transcript, stderr = client.finish_with_standard_error()
                    assert stderr.count("transcript recording disabled:") == 1, stderr
                    assert "failed to create" in stderr, stderr
                    return [
                        *transcript[:3],
                        {
                            "capture": "unpublished startup log is unavailable; computation survives"
                        },
                    ]


@requires(NATIVE_FIXTURES, POSIX, R)
def test_failed_publication_does_not_advertise_omitted_startup_images(
    binary: Path,
) -> Transcript:
    from support.r import r_test_environment

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        with closing(FifoCheckpoint.create(root / "images-retained")) as retained:
            # fmt: r
            startup = code("""
                options(
                  console.plot.width_in = 12,
                  console.plot.height_in = 12,
                  console.plot.dpi = 100
                )
                set.seed(1)
                for (i in 1:2) {
                  par(mar = rep(0, 4), xaxs = "i", yaxs = "i")
                  plot.new()
                  rasterImage(
                    matrix(rgb(runif(1200^2), runif(1200^2), runif(1200^2)), nrow = 1200),
                    0,
                    0,
                    1,
                    1,
                    interpolate = FALSE
                  )
                }
                dev.off()
                cat("startup images observed\\n")
                """)
            environment, _ = r_test_environment()
            config = root / ".agents/console/config.yaml"
            config.parent.mkdir(parents=True)
            config.write_text(
                json.dumps({"startup": {"language": "r", "code": startup}})
            )
            environment.update(
                {
                    "RETICULATE_PYTHON": sys.executable,
                    LOADER_VARIABLE: str(
                        build_interposer(root, "recording_observation")
                    ),
                    "MCP_CONSOLE_TEST_OBSERVED_TEXT": "startup images observed\n",
                    "MCP_CONSOLE_TEST_JOURNAL_OBSERVED": str(retained.path),
                }
            )
            with McpClient(binary, DIRECT.serve(), environment, root) as client:
                client.initialize_and_list_tools()
                retained.wait(
                    "startup images processed before publication",
                    timeout=600,
                    client=client,
                )
                (root / ".agents/console/sessions").write_text("not a directory")
                result = client.send(session="default")
                images = [c for c in result["content"] if c["type"] == "image"]
                assert len(images) == 1, [c["type"] for c in result["content"]]
                assert 4 * 1024 * 1024 < len(images[0]["data"]) <= 8 * 1024 * 1024
                assert base64.b64decode(images[0]["data"]).startswith(
                    b"\x89PNG\r\n\x1a\n"
                )
                text = "".join(
                    c["text"] for c in result["content"] if c["type"] == "text"
                )
                assert "1 image not retained" in text, text
                assert "artifacts/" not in text and "retained image:" not in text, text
                assert not result["isError"], result
                transcript, stderr = client.finish_with_standard_error()
                assert stderr.count("transcript recording disabled:") == 1, stderr
                return [
                    *transcript[:3],
                    {
                        "capture": "unpublished startup images are not advertised as retained"
                    },
                ]


@requires(NATIVE_FIXTURES, POSIX)
def test_worker_replacement_does_not_join_a_stalled_lifecycle_writer(
    binary: Path,
) -> Transcript:
    return replacement_with_stalled_journal(
        binary, "worker_ended", {"control": "restart"}
    )


@requires(NATIVE_FIXTURES, POSIX)
def test_worker_replacement_does_not_join_a_stalled_cell_start(
    binary: Path,
) -> Transcript:
    return replacement_with_stalled_journal(
        binary, "cell_started", {"r": "exit unexpectedly"}
    )


@requires(NATIVE_FIXTURES, POSIX)
def test_worker_replacement_does_not_join_a_stalled_text_event(
    binary: Path,
) -> Transcript:
    return replacement_with_stalled_journal(
        binary, "output_text", {"r": "exit after output"}
    )


@requires(NATIVE_FIXTURES, POSIX)
def test_worker_replacement_does_not_join_a_stalled_cell_end(
    binary: Path,
) -> Transcript:
    return replacement_with_stalled_journal(
        binary, "cell_ended", {"r": "exit unexpectedly"}
    )


def replacement_with_stalled_journal(
    binary: Path, event: str, arguments: dict
) -> Transcript:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        with (
            closing(FifoCheckpoint.create(root / "ended")) as ended,
            closing(FifoCheckpoint.create(root / "release")) as release,
        ):
            environment = {
                **os.environ,
                LOADER_VARIABLE: str(build_interposer(root, "recording_observation")),
                "MCP_CONSOLE_TEST_OBSERVED_EVENT": event,
                "MCP_CONSOLE_TEST_JOURNAL_OBSERVED": str(ended.path),
                "MCP_CONSOLE_TEST_JOURNAL_RELEASE": str(release.path),
                "TMPDIR": str(root),
                "ZOD_REPORT_PID": "1",
            }
            with McpClient(
                binary,
                DIRECT.serve(
                    "--worker",
                    str(Path(__file__).resolve().parents[3] / "fixtures/zod"),
                ),
                environment,
                root,
            ) as client:
                client.initialize_and_list_tools()
                assert not client.send(control="restart")["isError"]
                pid = root / "zod-worker-pid"
                previous = pid.read_text()
                restart = client.start_send(**arguments)
                try:
                    ended.wait(f"{event} journal write is stalled", client=client)
                    wait_for_checkpoint(
                        lambda: pid if pid.read_text() != previous else None,
                        f"replacement acquired while {event} journal write is stalled",
                        root=pid,
                        client=client,
                    )
                    # The stalled writer cannot record a delivery.
                    client.recording_receipts = False
                    client.receive(restart)
                    assert restart["result"]["isError"] == ("r" in arguments), restart
                    assert client.request("ping")["result"] == {}
                finally:
                    release.release()
                client.recording_receipts = True
                transcript = client.finish()
            (recording,) = (root / ".agents/console/sessions").iterdir()
            events = recording_events(recording)
            lifecycle = [
                (e["event"], e["session_id"])
                for e in events
                if e["event"].startswith("worker_")
            ]
            assert lifecycle == [
                (kind, session)
                for session in (1, 2)
                for kind in ("worker_started", "worker_ready", "worker_ended")
            ], lifecycle
            assert events[-1]["event"] == "recording_ended"
            return [
                *transcript[:3],
                {
                    "capture": f"worker retirement, replacement, and the reply do not wait for {event} journal I/O"
                },
            ]


if __name__ == "__main__":
    run_this_suite(__file__)
