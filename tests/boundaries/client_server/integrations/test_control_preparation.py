"""Human requirements use normal preparation without claiming Console output."""

import json
import sys
import tempfile
import time
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.integrations.test_control import (
    ControlClient,
    wait_for_pending_text,
)
from boundaries.client_server.python.test_without_r import (
    environment as sans_r_environment,
)
from support.checkpoints import FifoCheckpoint
from support.client import McpClient
from support.execution import DIRECT, SANDBOXED, Execution, executions
from support.normalization import (
    code,
    normalize_python_resolution_error,
    normalize_recording,
)
from support.native import LOADER_VARIABLE, build_interposer
from support.python import write_test_wheel
from support.r import r_test_environment
from support.records import TranscriptWithCompanions
from support.requirements import NATIVE_FIXTURES, POSIX, R, requires
from support.resolvers import expose_uv, recording_uv_environment
from support.suites import run_this_suite


def text(result: dict) -> str:
    return "".join(block.get("text", "") for block in result["content"])


@requires(NATIVE_FIXTURES, POSIX)
@executions(DIRECT)
def test_preparation_waits_for_mcp_delivery_acknowledgment(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-prepare-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        expose_uv(root)
        index = write_test_wheel(root, "delivery_probe", "value = 42\n")
        endpoint = root / "control.sock"
        with (
            closing(FifoCheckpoint.create(root / "ack-reached")) as reached,
            closing(FifoCheckpoint.create(root / "ack-release")) as release,
            McpClient(
                binary,
                execution.serve("--control-socket", str(endpoint)),
                {
                    **sans_r_environment(root),
                    "UV_INDEX": index.as_uri(),
                    "UV_INDEX_STRATEGY": "first-index",
                    LOADER_VARIABLE: str(
                        build_interposer(root, "control_response_ack")
                    ),
                    "MCP_CONSOLE_TEST_ACK_MATCH": "delivered before acknowledgment",
                    "MCP_CONSOLE_TEST_ACK_REACHED": str(reached.path),
                    "MCP_CONSOLE_TEST_ACK_RELEASE": str(release.path),
                },
                root,
            ) as client,
        ):
            client.transcript.append(
                {
                    "scenario": "Nonwaiting preparation needs completed MCP delivery, not only received result bytes.",
                    "fixture": "A native checkpoint holds the response write after its full frame reaches the client and before delivery acknowledgment.",
                }
            )
            client.initialize_and_list_tools()
            try:
                # The held write returns only on release, so the recording
                # cannot yet hold this reply's delivery.
                client.recording_receipts = False
                client.expect(
                    "delivered before acknowledgment\n",
                    python="print('delivered before acknowledgment')",
                )
                client.recording_receipts = True
                reached.wait(
                    "completed MCP response awaits acknowledgment", client=client
                )
                with ControlClient(endpoint) as control:
                    pending = control.request(op="status")["result"]
                    assert pending["state"] == "awaiting_result", pending
                    busy = control.request(
                        op="prepare", requirements={"python": ["delivery-probe"]}
                    )["result"]
                    assert busy["status"] == "busy", busy
                    assert "delivery-probe" not in busy["requirements"]["python"], busy
                    release.release()
                    control.wait_for_idle(client)
                    prepared = control.request(
                        op="prepare",
                        requirements={"python": ["delivery-probe"]},
                        response_timeout=max(
                            0, client.response_deadline() - time.monotonic()
                        ),
                    )["result"]
                    assert prepared["status"] == "prepared", prepared
                    assert prepared["worker_id"] == busy["worker_id"], prepared
                    assert "delivery-probe" in prepared["requirements"]["python"], (
                        prepared
                    )
                    client.expect(
                        "42\n",
                        python="import delivery_probe; print(delivery_probe.value)",
                    )
                    transcript = client.finish()
                return TranscriptWithCompanions(
                    normalize_recording(transcript, root),
                    {"control.yaml": normalize_recording(control.transcript, root)},
                )
            finally:
                release.release()


@requires(POSIX, R)
@executions(DIRECT, SANDBOXED)
def test_preparation_rejects_incomplete_interpreter_startup(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-prepare-"
    ) as directory:
        root = Path(directory)
        endpoint = root / "control.sock"
        environment, _ = r_test_environment()
        profile = root / ".Rprofile"
        environment["R_PROFILE_USER"] = str(profile)
        arguments = ["--control-socket", str(endpoint)]
        if execution == SANDBOXED:
            arguments += ["--writable-root", str(root)]
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            closing(FifoCheckpoint.create(root / "resume")) as resume,
        ):
            # fmt: r
            source = code(f"""
                local({{
                  wait <- function(path) {{
                    gate <- fifo(path, "rb", blocking = TRUE)
                    stopifnot(identical(readBin(gate, "raw", n = 1L), charToRaw("1")))
                    close(gate)
                  }}
                  gate <- fifo({json.dumps(str(entered.path))}, "wb", blocking = TRUE)
                  writeBin(charToRaw("1"), gate)
                  close(gate)
                  wait({json.dumps(str(release.path))})
                  cat("native startup output\\n")
                  wait({json.dumps(str(resume.path))})
                }})
                """)
            profile.write_text(source)
            with McpClient(
                binary,
                execution.serve(*arguments),
                environment,
                root,
                use_r_startup_files=True,
            ) as client:
                client.initialize_and_list_tools()
                try:
                    entered.wait(timeout=client.response_timeout, client=client)
                    with ControlClient(endpoint) as control:
                        # R has printed nothing yet. Inspection reports `busy`
                        # while the server routes worker output, so startup
                        # output waits until these observations are complete.
                        # Create observes configuration readiness without waiting
                        # for the interpreter, excluding the earlier startup gate.
                        configured = control.request(op="create", session="default")[
                            "result"
                        ]
                        assert configured["session"] == "default", configured
                        assert configured["worker_id"] == 1, configured
                        status = control.request(op="status")["result"]
                        assert status == {
                            "state": "starting",
                            "idle": False,
                            "evaluation": None,
                        }, status
                        baseline = control.request(op="requirements")["result"]
                        busy = control.request(
                            op="prepare", requirements={"r": ["praise"]}
                        )["result"]
                        assert busy["status"] == "busy", busy
                        assert {key: busy[key] for key in baseline} == baseline, busy
                        release.release()
                        # Only output the server has received becomes the
                        # cell's idle prelude.
                        wait_for_pending_text(endpoint, "native startup output\n")
                        resume.release()
                        client.expect(
                            "native startup output\n[output produced while idle]\nready\n",
                            r='cat("ready\\n")',
                        )
                        control.wait_for_idle(client)
                        prepared = control.request(
                            op="prepare",
                            requirements={"r": ["praise"]},
                            response_timeout=max(
                                0, client.response_deadline() - time.monotonic()
                            ),
                        )["result"]
                        assert prepared["status"] == "prepared", prepared
                        assert prepared["worker_id"] == configured["worker_id"], (
                            prepared
                        )
                        assert "praise" in prepared["requirements"]["r"], prepared
                        client.expect(
                            r="stopifnot(requireNamespace('praise', quietly = TRUE))"
                        )
                        client.expect("\n[idle]")
                        transcript = client.finish()
                    return TranscriptWithCompanions(
                        transcript, {"control.yaml": control.transcript}
                    )
                finally:
                    release.release()
                    resume.release()


@requires(POSIX, R)
@executions(DIRECT, SANDBOXED)
def test_prepares_named_requirements_before_start_and_after_stop(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-prepare-"
    ) as directory:
        root = Path(directory)
        endpoint = root / "control.sock"
        with McpClient(
            binary,
            execution.serve("--control-socket", str(endpoint)),
            current_directory=root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(r="invisible(NULL)")
            with ControlClient(endpoint) as control:
                created = control.request(op="create", session="analysis")["result"]
                assert created["state"] == "stopped" and created["worker_id"] == 0, (
                    created
                )
                for package, worker in (("praise", 0), ("cli", 1)):
                    prepared = control.request(
                        op="prepare",
                        session="analysis",
                        requirements={"r": [package]},
                        response_timeout=max(
                            0, client.response_deadline() - time.monotonic()
                        ),
                    )["result"]
                    assert prepared["status"] == "prepared", prepared
                    assert prepared["worker_id"] == worker, prepared
                    assert package in prepared["requirements"]["r"], prepared
                    listed = control.request(op="sessions")["result"]["sessions"]
                    selected = next(
                        item for item in listed if item["name"] == "analysis"
                    )
                    assert selected["state"] == "stopped", selected
                    assert selected["worker_id"] == worker, selected
                    result = client.send(
                        session="analysis",
                        r=f'stopifnot(requireNamespace("{package}", quietly = TRUE)); cat("named requirements ready\\n")',
                    )
                    assert "named requirements ready\n" in text(result), result
                    stopped = control.request(op="close", session="analysis")["result"]
                    assert stopped["accepted"], stopped
                transcript = client.finish()
            return TranscriptWithCompanions(
                transcript, {"control.yaml": control.transcript}
            )


@requires(POSIX, R)
@executions(DIRECT, SANDBOXED)
def test_prepares_live_requirements_without_replacing_session(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-prepare-"
    ) as directory:
        root = Path(directory)
        endpoint = root / "control.sock"
        with McpClient(
            binary,
            execution.serve("--control-socket", str(endpoint)),
            current_directory=root,
        ) as client:
            client.initialize_and_list_tools()
            client.expect(r="kept_r <- new.env(); kept_r$value <- 42L")
            client.expect(python="kept_python = {'value': 42}")
            with ControlClient(endpoint) as control:
                control.wait_for_idle(client)
                worker = control.request(op="output")["result"]["worker_id"]
                prepared = control.request(
                    op="prepare",
                    requirements={
                        "r": ["praise"],
                        "python": ["more-itertools"],
                        "duckdb": ["json"],
                    },
                    response_timeout=max(
                        0, client.response_deadline() - time.monotonic()
                    ),
                )["result"]
                assert prepared["status"] == "prepared", prepared
                assert prepared["worker_id"] == worker, prepared
                assert prepared["prepared"], prepared
                assert "praise" in prepared["requirements"]["r"], prepared
                assert "more-itertools" in prepared["requirements"]["python"], prepared
                assert "json" in prepared["requirements"]["duckdb"], prepared
                inspected = control.request(op="requirements")["result"]
                assert {key: prepared[key] for key in inspected} == inspected, prepared
                client.expect(
                    r="stopifnot(kept_r$value == 42L, requireNamespace('praise', quietly = TRUE))"
                )
                client.expect(
                    python="import more_itertools; assert kept_python['value'] == 42; assert more_itertools.one([42]) == 42"
                )
                client.expect("\n[idle]")
                for action in ("get", "set", "reset"):
                    rejected = control.request(
                        op="prepare", requirements={"action": action}
                    )
                    assert rejected["error"]["code"] == "invalid_request", rejected
                transcript = client.finish()
            return TranscriptWithCompanions(
                transcript, {"control.yaml": control.transcript}
            )


@requires(POSIX, R)
@executions(DIRECT, SANDBOXED)
def test_busy_preparation_preserves_private_evaluation(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-prepare-"
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
            client.expect(python="pass")
            with (
                ControlClient(endpoint) as control,
                ControlClient(endpoint) as evaluator,
            ):
                baseline = control.request(op="requirements")["result"]
                # fmt: python
                source = code(f"""
                    print("private evaluation output", flush=True)
                    with open({str(entered.path)!r}, "wb", buffering=0) as gate:
                        gate.write(b"1")
                    with open({str(release.path)!r}, "rb", buffering=0) as gate:
                        gate.read(1)
                    """)
                pending = evaluator.start(
                    op="evaluate",
                    language="python",
                    code=source,
                    busy="wait",
                    wait_ms=30000,
                )
                try:
                    entered.wait(client=client)
                    busy = control.request(
                        op="prepare", requirements={"python": ["more-itertools"]}
                    )["result"]
                    assert busy["status"] == "busy", busy
                    assert {key: busy[key] for key in baseline} == baseline, busy
                finally:
                    release.release()
                completed = evaluator.receive(pending)["result"]
                assert text(completed) == "private evaluation output\n", completed
                client.expect("\n[idle]")
                transcript = client.finish()
            return TranscriptWithCompanions(
                transcript,
                {
                    "control.yaml": control.transcript,
                    "evaluation.yaml": normalize_recording(evaluator.transcript, root),
                },
            )


@requires(POSIX, R)
@executions(DIRECT, SANDBOXED)
def test_failed_preparation_retains_manifest_state_and_mcp_output(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-prepare-"
    ) as directory:
        root = Path(directory)
        endpoint = root / "control.sock"
        candidate = "mcp-console-control-prepare-failure"
        environment, _ = recording_uv_environment(root, fail_requirement=candidate)
        with (
            closing(FifoCheckpoint.create(root / "release")) as release,
            McpClient(
                binary,
                execution.serve("--control-socket", str(endpoint), "-c", "cache=host"),
                environment,
                root,
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.expect(python="kept = {'value': 42}")
            executable_result = client.send(python="import sys; print(sys.executable)")
            executable = text(executable_result).strip()
            assert Path(executable).is_absolute(), executable
            executable_result["content"][0]["text"] = "<running Python>\n"
            with (
                ControlClient(endpoint) as control,
                ControlClient(endpoint) as observer,
            ):
                baseline = control.request(op="requirements")["result"]
                observer.request(op="subscribe", format="events")
                json.loads(observer.reader.readline(timeout=30))
                # fmt: python
                source = code(f"""
                    import os
                    import threading

                    def background_output():
                        with open({str(release.path)!r}, "rb", buffering=0) as gate:
                            gate.read(1)
                        os.write(1, b"uncollected MCP output\\n")

                    threading.Thread(target=background_output).start()
                    """)
                client.expect(python=source)
                control.wait_for_idle(client)
                release.release()
                while True:
                    snapshot = json.loads(observer.reader.readline(timeout=30))[
                        "result"
                    ]
                    if any(
                        "uncollected MCP output" in event.get("text", "")
                        for event in snapshot["events"]
                    ):
                        break
                failed = control.request(
                    op="prepare",
                    requirements={"python": [candidate]},
                    response_timeout=max(
                        0, client.response_deadline() - time.monotonic()
                    ),
                )["result"]
                assert failed["status"] == "failed", failed
                assert "synthetic uv failure" in failed["message"], failed
                assert "uncollected MCP output" not in failed["message"], failed
                assert {key: failed[key] for key in baseline} == baseline, failed
                failed["message"] = normalize_python_resolution_error(
                    failed["message"], executable=executable
                )
                collected = client.send()
                assert text(collected).count("uncollected MCP output") == 1, collected
                for block in collected["content"]:
                    if (
                        block["type"] == "text"
                        and "synthetic uv failure" in block["text"]
                    ):
                        block["text"] = normalize_python_resolution_error(
                            block["text"], executable=executable
                        )
                client.expect("\n[idle]")
                client.expect("42\n", python="print(kept['value'])")
                transcript = normalize_recording(client.finish(), root)
            return TranscriptWithCompanions(
                transcript,
                {"control.yaml": normalize_recording(control.transcript, root)},
            )


if __name__ == "__main__":
    run_this_suite(__file__)
