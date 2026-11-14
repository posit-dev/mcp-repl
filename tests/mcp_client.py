#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["py-yaml12>=0.2.0"]
# ///

from __future__ import annotations

import json
import os
import runpy
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from collections.abc import Callable, Iterator
from concurrent.futures import CancelledError, ThreadPoolExecutor
from contextlib import closing, contextmanager, nullcontext
from pathlib import Path
from types import SimpleNamespace
from threading import Event
from typing import Any
from unittest.mock import patch

from support.assertions import wait_for_evaluation_output, wait_for_idle_output
from support.checkpoints import (
    FifoCheckpoint,
    release_fixture_checkpoint,
    release_partial_sideband,
    wait_for_checkpoint,
    wait_for_path,
    wait_for_worker_file,
)
from support.client import McpClient
from support.events import Events
from support.normalization import code
from support.processes import capture_process_identity, signal_process
from support.requirements import POSIX, PROCESS_EVENTS

ROOT = Path(__file__).resolve().parent.parent

# fmt: python
FAKE_SERVER = r"""
import json
import os
import sys
from pathlib import Path

mode = sys.argv[1]
root = Path(sys.argv[2])
with (root / "ready").open("wb", buffering=0) as ready:
    assert ready.write(b"1") == 1

if mode == "partial":
    json.loads(sys.stdin.readline())
    os.write(2, b"partial response diagnostic\n")
    os.write(1, b'{"jsonrpc":"2.0","id":')
elif mode == "finish":
    assert sys.stdin.read() == ""
    os.write(2, b"shutdown diagnostic\n")
elif mode in {"context", "refuse-close", "late-response"}:
    request = json.loads(sys.stdin.readline())
    response = {
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {"content": [{"type": "text", "text": "ready"}], "isError": False},
    }
    print(json.dumps(response), flush=True)
    if mode == "late-response":
        json.loads(sys.stdin.readline())
        os.write(2, b"partial response diagnostic\n")
        os.write(1, b'{"jsonrpc":"2.0","id":')
    else:
        assert sys.stdin.read() == ""
        (root / "stdin-closed").touch()
        if mode == "context":
            raise SystemExit(0)
        os.write(2, b"server refuses input closure\n")
        with (root / "shutdown-started").open("wb", buffering=0) as started:
            assert started.write(b"1") == 1
else:
    raise AssertionError(mode)

with (root / "release").open("rb", buffering=0) as release:
    assert release.read(1) == b"1"
""".lstrip()

# fmt: python
RUNNER_CLIENT_SUITE = """
import sys
from pathlib import Path

from support.client import McpClient


def test_waits_with_client(binary: Path) -> list[dict[str, str]]:
    root = binary.parents[2]
    client = McpClient(
        Path(sys.executable),
        ("-u", str(root / "server.py"), "refuse-close", str(root)),
        current_directory=root,
        recording_receipts=False,
    )
    (root / "server-pid").write_text(str(client.process.pid))
    try:
        with client:
            assert client.send(r="1")["content"][0]["text"] == "ready"
            with (root / "case-started").open("wb", buffering=0) as started:
                assert started.write(b"1") == 1
            with (root / "case-release").open("rb", buffering=0) as release:
                assert release.read(1) == b"1"
    finally:
        (root / "client-closed").write_text(str(client.process.returncode))
        with (root / "case-cleanup-complete").open("wb", buffering=0) as cleaned:
            assert cleaned.write(b"1") == 1
    return []
""".lstrip()

# fmt: python
LATE_RESPONSE_SUITE = """
import sys
from pathlib import Path

from support.client import McpClient


def test_waits_with_client(binary: Path) -> list[dict[str, str]]:
    root = binary.parents[2]
    client = McpClient(
        Path(sys.executable),
        ("-u", str(root / "server.py"), "late-response", str(root)),
        current_directory=root,
        response_timeout=60,
        shutdown_timeout=0.1,
        recording_receipts=False,
    )
    try:
        with client:
            assert client.send(r="1")["content"][0]["text"] == "ready"
            with (root / "case-started").open("wb", buffering=0) as started:
                assert started.write(b"1") == 1
            client.send(r="2")
    finally:
        with (root / "case-cleanup-complete").open("wb", buffering=0) as cleaned:
            assert cleaned.write(b"1") == 1
    return []
""".lstrip()


# fmt: python
STALLED_CHECKPOINT_SUITE = """
import select
import sys
import time
from pathlib import Path

from support.client import McpClient


def test_waits_with_client(binary: Path) -> list[dict[str, str]]:
    root = binary.parents[2]
    client = McpClient(
        Path(sys.executable),
        ("-u", str(root / "server.py"), "refuse-close", str(root)),
        current_directory=root,
        shutdown_timeout=0.1,
        recording_receipts=False,
    )
    try:
        with client:
            assert client.send(r="1")["content"][0]["text"] == "ready"
            with (root / "case-started").open("wb", buffering=0) as started:
                assert started.write(b"1") == 1
            with (root / "case-release").open("rb", buffering=0) as checkpoint:
                readable, _, _ = select.select(
                    [checkpoint, client.stdout],
                    [],
                    [],
                    max(0, client.response_deadline() - time.monotonic()),
                )
                assert readable, "resolver startup checkpoint deadline expired"
    finally:
        (root / "client-closed").write_text(str(client.process.returncode))
        with (root / "case-cleanup-complete").open("wb", buffering=0) as cleaned:
            assert cleaned.write(b"1") == 1
    return []
""".lstrip()


@unittest.skipUnless(POSIX.available, POSIX.reason)
class McpClientTests(unittest.TestCase):
    @contextmanager
    def client_runner(
        self, suite: str, *arguments: str
    ) -> Iterator[tuple[subprocess.Popen[str], Path, list[int]]]:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = root / "tests" / "boundaries" / "_run.py"
            suite_path = runner.parent / "client_server" / "server" / "test_client.py"
            snapshots = (
                root
                / "tests"
                / "snapshots"
                / "client_server"
                / "server"
                / "test_client"
            )
            support = root / "tests" / "support"
            binary = root / "target" / "release" / "mcp-console"
            for path in (suite_path.parent, snapshots, support, binary.parent):
                path.mkdir(parents=True, exist_ok=True)
            for name in ("_run.py", "_profiles.py"):
                shutil.copy2(
                    ROOT / "tests" / "boundaries" / name, runner.with_name(name)
                )
            for name in (
                "__init__.py",
                "cases.py",
                "client.py",
                "records.py",
                "snapshots.py",
                "progress.py",
                "requirements.py",
                "linux_sandbox.py",
                "execution.py",
            ):
                shutil.copy2(ROOT / "tests" / "support" / name, support / name)
            binary.touch()
            suite_path.write_text(suite)
            (snapshots / "waits_with_client.yaml").write_text("[]\n")
            (root / "server.py").write_text(FAKE_SERVER)
            checkpoints = []
            for name in (
                "ready",
                "release",
                "case-started",
                "case-release",
                "shutdown-started",
                "case-cleanup-complete",
            ):
                os.mkfifo(root / name)
                checkpoints.append(os.open(root / name, os.O_RDWR | os.O_NONBLOCK))
            process = subprocess.Popen(
                [sys.executable, runner, "--full", "--jobs", "1", *arguments],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
                cwd=root,
                env=os.environ | {"MCP_CONSOLE_TEST_BINARY": str(binary)},
            )
            try:
                yield process, root, checkpoints
            finally:
                try:
                    os.write(checkpoints[1], b"1")
                    os.write(checkpoints[3], b"1")
                    try:
                        process.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=5)
                    ready, _, _ = select.select([checkpoints[5]], [], [], 5)
                    self.assertTrue(ready, "case did not finish its client cleanup")
                    self.assertEqual(os.read(checkpoints[5], 1), b"1")
                finally:
                    for checkpoint in checkpoints:
                        os.close(checkpoint)

    @contextmanager
    def fake_client(
        self, mode: str, **timeouts: float
    ) -> Iterator[tuple[McpClient, Path]]:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkpoints = []
            for name in ("ready", "release"):
                os.mkfifo(root / name)
                checkpoints.append(os.open(root / name, os.O_RDWR | os.O_NONBLOCK))
            client = McpClient(
                Path(sys.executable),
                ("-u", "-c", FAKE_SERVER, mode, str(root)),
                current_directory=root,
                # The fake server writes no recording.
                recording_receipts=False,
                **timeouts,
            )
            try:
                ready, _, _ = select.select([checkpoints[0]], [], [], 10)
                self.assertTrue(ready, "fake server did not start")
                self.assertEqual(os.read(checkpoints[0], 1), b"1")
                yield client, root
            finally:
                os.write(checkpoints[1], b"1")
                if client.process.poll() is None:
                    client.process.kill()
                client.process.wait(timeout=5)
                for stream in (client.stdin, client.stdout, client.stderr):
                    stream.close()
                for checkpoint in checkpoints:
                    os.close(checkpoint)

    def call_bounded(self, client: McpClient, call: Callable[[], object]) -> object:
        def capture() -> object:
            try:
                return call()
            except (AssertionError, RuntimeError, TimeoutError) as error:
                return error

        with ThreadPoolExecutor(max_workers=1) as executor:
            result = executor.submit(capture)
            try:
                return result.result(timeout=5)
            finally:
                # Killing this fixture closes its partial streams before the
                # executor joins, so a broken client cannot hang this test.
                if not result.done():
                    client.process.kill()
                    client.process.wait(timeout=5)

    def test_partial_response_times_out_with_server_diagnostics(self) -> None:
        with self.fake_client("partial", response_timeout=1, shutdown_timeout=1) as (
            client,
            _,
        ):
            result = self.call_bounded(client, lambda: client.send(r="1"))
            self.assertIsInstance(result, TimeoutError)
            self.assertIn("response", str(result))
            self.assertIn("partial response diagnostic", str(result))

    def test_collector_deadline_bounds_transport_and_restores_budget(self) -> None:
        with self.fake_client("partial", response_timeout=60) as (client, _):
            started = time.monotonic()
            result = self.call_bounded(
                client,
                lambda: wait_for_evaluation_output(
                    client,
                    "ready",
                    "partial cell",
                    completion_timeout_seconds=0.05,
                    r="once",
                ),
            )
            self.assertIsInstance(result, TimeoutError)
            self.assertLess(time.monotonic() - started, 1)
            self.assertEqual(client.response_timeout, 60)
            self.assertEqual(
                [entry["send"] for entry in client.transcript], [{"r": "once"}]
            )

    def test_finish_times_out_with_server_diagnostics(self) -> None:
        with self.fake_client("finish", shutdown_timeout=1) as (client, _):
            result = self.call_bounded(client, client.finish)
            self.assertIsInstance(result, TimeoutError)
            self.assertIn("shutdown", str(result))
            self.assertIn("shutdown diagnostic", str(result))

    def test_context_exception_closes_stdin_and_reaps_server(self) -> None:
        with self.fake_client("context", shutdown_timeout=1) as (client, root):

            def fail_in_context() -> None:
                with client:
                    result = client.send(r="1")
                    self.assertEqual(
                        result["content"], [{"type": "text", "text": "ready"}]
                    )
                    raise AssertionError("scenario assertion failed")

            result = self.call_bounded(client, fail_in_context)
            self.assertIsInstance(result, AssertionError)
            self.assertEqual(str(result), "scenario assertion failed")
            self.assertEqual(client.process.returncode, 0)
            self.assertTrue((root / "stdin-closed").is_file())

    @unittest.skipUnless(PROCESS_EVENTS.available, PROCESS_EVENTS.reason)
    def test_runner_interrupt_allows_client_to_reap_unresponsive_server(self) -> None:
        with self.client_runner(RUNNER_CLIENT_SUITE, "--timeout", "60") as (
            process,
            root,
            checkpoints,
        ):
            identity = None
            exits = Events()
            try:
                ready, _, _ = select.select([checkpoints[2]], [], [], 10)
                self.assertTrue(ready, "case did not receive its server response")
                self.assertEqual(os.read(checkpoints[2], 1), b"1")
                pid = int((root / "server-pid").read_text())
                identity = capture_process_identity(pid)
                exits.watch_process(pid)
                process.send_signal(signal.SIGINT)
                ready, _, _ = select.select([checkpoints[4]], [], [], 10)
                self.assertTrue(ready, "case did not close server stdin")
                self.assertEqual(os.read(checkpoints[4], 1), b"1")
                # Server exit must leave room for the case to finish before
                # its supervisor's 15-second forced-cleanup deadline.
                observed = exits.wait(14)
                self.assertTrue(observed, "client left no time for case cleanup")
                self.assertEqual(observed, {pid})
                stdout, stderr = process.communicate(timeout=5)
                self.assertNotEqual(process.returncode, 0, stdout)
                self.assertEqual((root / "client-closed").read_text(), "-9")
                self.assertIn("KeyboardInterrupt", stderr)
            finally:
                if identity is not None:
                    signal_process(identity, signal.SIGKILL)
                exits.close()

    def test_later_response_reports_diagnostics_before_runner_deadline(self) -> None:
        with self.client_runner(LATE_RESPONSE_SUITE, "--timeout", "16") as (
            process,
            _,
            checkpoints,
        ):
            ready, _, _ = select.select([checkpoints[2]], [], [], 10)
            self.assertTrue(ready, "first response did not complete")
            self.assertEqual(os.read(checkpoints[2], 1), b"1")
            stdout, stderr = process.communicate(timeout=20)
            self.assertNotEqual(process.returncode, 0, stdout)
            self.assertIn("timed out waiting for response", stderr)
            self.assertIn("partial response diagnostic", stderr)
            self.assertNotIn("timed out after 16 seconds", stderr)

    def test_stalled_checkpoint_leaves_time_to_reap_server(self) -> None:
        with self.client_runner(STALLED_CHECKPOINT_SUITE, "--timeout", "16") as (
            process,
            root,
            checkpoints,
        ):
            ready, _, _ = select.select([checkpoints[2]], [], [], 10)
            self.assertTrue(ready, "first response did not complete")
            self.assertEqual(os.read(checkpoints[2], 1), b"1")
            stdout, stderr = process.communicate(timeout=20)
            self.assertNotEqual(process.returncode, 0, stdout)
            self.assertIn("resolver startup checkpoint deadline expired", stderr)
            self.assertNotIn("timed out after 16 seconds", stderr)
            self.assertEqual((root / "client-closed").read_text(), "-9")


class PortableMcpClientTests(unittest.TestCase):
    def test_isolates_console_home_without_changing_home_or_project_config(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            environment = os.environ | {
                "HOME": str(root / "caller-home"),
                "MCP_CONSOLE_HOME": str(root / "caller-console"),
            }
            # fmt: python
            source = code("""
                import json
                import os
                import sys
                from pathlib import Path

                values = {name: os.environ[name] for name in ("HOME", "MCP_CONSOLE_HOME")}
                Path("environment.json").write_text(json.dumps(values))
                assert sys.stdin.read() == ""
                """)
            for record_in_project in (False, True):
                with self.subTest(record_in_project=record_in_project):
                    workspace = root / str(record_in_project)
                    workspace.mkdir()
                    with McpClient(
                        Path(sys.executable),
                        ("-c", source),
                        environment=environment,
                        current_directory=workspace,
                        record_in_project=record_in_project,
                    ) as client:
                        client.finish()
                    actual = json.loads((workspace / "environment.json").read_text())
                    self.assertEqual(actual["HOME"], environment["HOME"])
                    self.assertNotEqual(
                        actual["MCP_CONSOLE_HOME"], environment["MCP_CONSOLE_HOME"]
                    )
                    self.assertFalse(
                        (workspace / ".agents/console/config.yaml").exists()
                    )

    def test_pipe_transport_records_responses_and_drains_diagnostics(self) -> None:
        # fmt: python
        source = code("""
            import json
            import sys

            request = json.loads(sys.stdin.readline())
            sys.stderr.write("diagnostic\\n" * 10000)
            sys.stderr.flush()
            print(
                json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"content": [{"type": "text", "text": "42"}], "isError": False},
                    }
                ),
                flush=True,
            )
            assert sys.stdin.read() == ""
            """)
        with McpClient(
            Path(sys.executable), ("-u", "-c", source), recording_receipts=False
        ) as client:
            result = client.send(python="6 * 7")
            self.assertEqual(result["content"][0]["text"], "42")
            transcript, diagnostics = client.finish_with_standard_error()
            self.assertEqual(diagnostics, ("diagnostic" + os.linesep) * 10000)
            self.assertEqual(transcript[0]["send"], {"python": "6 * 7"})
            self.assertEqual(client.process.returncode, 0)

    def test_recording_disabled_wait_requires_the_report(self) -> None:
        # fmt: python
        source = code("""
            import sys

            sys.stdin.readline()
            sys.stderr.write("mcp-console: transcript recording disabled: no space\\n")
            sys.stderr.flush()
            assert sys.stdin.read() == ""
            """)
        with McpClient(
            Path(sys.executable),
            ("-u", "-c", source),
            response_timeout=1,
            recording_receipts=False,
        ) as client:
            # Opting out of receipts is not a report that recording stopped.
            with self.assertRaises(TimeoutError):
                client.await_recording_disabled()
            client.notify("notifications/cancelled", requestId=1)
            client.await_recording_disabled()
            _, diagnostics = client.finish_with_standard_error()
            self.assertEqual(
                diagnostics,
                "mcp-console: transcript recording disabled: no space" + os.linesep,
            )

    def test_reply_does_not_wait_for_a_disabled_recording(self) -> None:
        # fmt: python
        source = code("""
            import json
            import sys

            request = json.loads(sys.stdin.readline())
            print(
                json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"content": [{"type": "text", "text": "42"}], "isError": False},
                    }
                ),
                flush=True,
            )
            sys.stderr.write("mcp-console: transcript recording disabled: no space\\n")
            sys.stderr.flush()
            assert sys.stdin.read() == ""
            """)
        with McpClient(
            Path(sys.executable), ("-u", "-c", source), response_timeout=5
        ) as client:
            # No journal ever records this delivery; the report ends the wait.
            result = client.send(python="6 * 7")
            self.assertEqual(result["content"][0]["text"], "42")
            self.assertFalse(client.recording_receipts)
            _, diagnostics = client.finish_with_standard_error()
            self.assertEqual(
                diagnostics,
                "mcp-console: transcript recording disabled: no space" + os.linesep,
            )

    @unittest.skipUnless(
        sys.platform == "win32",
        "only Windows holds a file exclusively while copying it into place",
    )
    def test_reply_waits_for_a_journal_that_is_being_published(self) -> None:
        # fmt: python
        source = code("""
            import ctypes
            import json
            import sys
            import time
            from ctypes import wintypes
            from pathlib import Path

            request = json.loads(sys.stdin.readline())
            pid = int(request["params"]["arguments"]["python"])
            journal = Path(f".agents/console/sessions/run-{pid:010}/capture/events.jsonl")
            journal.parent.mkdir(parents=True)
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.CreateFileW.restype = wintypes.HANDLE
            kernel32.CreateFileW.argtypes = [
                wintypes.LPCWSTR,
                wintypes.DWORD,
                wintypes.DWORD,
                wintypes.LPVOID,
                wintypes.DWORD,
                wintypes.DWORD,
                wintypes.HANDLE,
            ]
            kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
            # GENERIC_WRITE, no sharing, CREATE_NEW: the destination of a copy.
            handle = kernel32.CreateFileW(str(journal), 0x40000000, 0, None, 1, 0x80, None)
            assert handle != wintypes.HANDLE(-1).value, ctypes.get_last_error()
            print(
                json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"content": [{"type": "text", "text": "42"}], "isError": False},
                    }
                ),
                flush=True,
            )
            # The client cannot report a refused open. Keep the journal held
            # long enough for the client's path discovery to meet it.
            time.sleep(0.5)
            assert kernel32.CloseHandle(handle)
            with journal.open("a") as stream:
                stream.write(json.dumps({"event": "delivery", "request_id": request["id"]}) + "\\n")
            assert sys.stdin.read() == ""
            """)
        with McpClient(
            Path(sys.executable), ("-u", "-c", source), response_timeout=5
        ) as client:
            # The server's process ID names its recording directory.
            pending = client.start_send(python=str(client.process.pid))
            client.receive(pending)
            self.assertEqual(pending["result"]["content"][0]["text"], "42")
            client.finish()

    def test_partial_pipe_response_has_a_bounded_diagnostic_timeout(self) -> None:
        # fmt: python
        source = code("""
            import sys

            sys.stdin.readline()
            sys.stderr.write("partial response diagnostic\\n")
            sys.stderr.flush()
            sys.stdout.write('{"jsonrpc":"2.0","id":')
            sys.stdout.flush()
            sys.stdin.read()
            """)
        with McpClient(
            Path(sys.executable), ("-u", "-c", source), response_timeout=0.5
        ) as client:
            with self.assertRaisesRegex(TimeoutError, "partial response diagnostic"):
                client.send(python="6 * 7")
        self.assertIsNotNone(client.process.returncode)


class ScriptedClient:
    response_timeout = 600

    def __init__(self, responses: list[dict[str, object]]) -> None:
        self.responses = iter(responses)
        self.calls = []
        self.transcript = []

    def send(self, **arguments: object) -> dict[str, object]:
        result = next(self.responses)
        self.calls.append(arguments)
        self.transcript.append({"send": arguments, "result": result})
        return result


class EvaluationCollectorTests(unittest.TestCase):
    def test_idle_output_collects_split_startup_diagnostics(self) -> None:
        first = "Error: selected Python executable is missing\n"
        second = "Error: Python initialization is incomplete; SQL withheld\n"
        expected = first + second + "\n[idle]"
        client = ScriptedClient(
            [
                {"content": [{"type": "text", "text": output}], "isError": False}
                for output in (first + "\n[idle]", "\n[idle]", second + "\n[idle]")
            ]
        )
        wait_for_idle_output(client, expected, "split startup diagnostics")
        self.assertEqual(client.calls, [{"session": "default"}] * 3)
        self.assertEqual(len(client.transcript), 1)
        self.assertEqual(client.transcript[0]["result"]["content"][0]["text"], expected)

    def test_idle_output_waits_for_startup_input(self) -> None:
        expected = '[input requested: "startup> "]\n[waiting for stdin]'
        client = ScriptedClient(
            [
                {"content": [{"type": "text", "text": output}], "isError": False}
                for output in ("\n[idle]", "\n[idle]", expected)
            ]
        )
        wait_for_idle_output(
            client,
            expected,
            "startup input",
            completion_timeout_seconds=client.response_timeout,
        )
        self.assertEqual(client.calls, [{"session": "default"}] * 3)
        self.assertEqual(len(client.transcript), 1)
        self.assertEqual(client.transcript[0]["result"]["content"][0]["text"], expected)
        self.assertEqual(client.response_timeout, 600)

    def test_observes_running_and_stdin_without_waiting_for_completion(self) -> None:
        running = "\n[running; poll with an empty send]"
        for state in ("arrived\n" + running, "prompt\n[waiting for stdin]"):
            with self.subTest(state=state):
                client = ScriptedClient(
                    [
                        {"content": [{"type": "text", "text": running}]},
                        {"content": [{"type": "text", "text": state}]},
                    ]
                )
                self.assertEqual(
                    wait_for_evaluation_output(
                        client,
                        state,
                        "state arrival",
                        completion_timeout_seconds=1,
                        python="once",
                        timeout_ms=0,
                    ),
                    state,
                )
                # A running cell cannot complete to wake a long receive. Leave
                # budget for its snapshot to reach the client before the deadline.
                self.assertLess(client.calls[1]["timeout_ms"], 500)

    def test_exact_deltas_terminal_states_and_single_submission(self) -> None:
        running = "\n[running; poll with an empty send]"
        rows = (
            (["one" + running, "two" + running, "[done]"], "onetwo", False),
            ([running, "[done]"], "[done]", False),
            (["one" + running, "two"], "onetwo", False),
            (
                [
                    "\n[waiting for stdin]",
                    "fresh " + running,
                    "input\n" + running,
                    "[done]",
                ],
                "fresh input\n",
                False,
            ),
            (
                ["prompt\n" + running, "\n[waiting for stdin]"],
                "prompt\n[waiting for stdin]",
                False,
            ),
            (["one" + running], "one" + running, False),
            (["exact resolver error\n"], "exact resolver error\n", True),
        )
        source = {"python": "once", "control": "restart", "stdin": "payload"}
        for deltas, expected, error in rows:
            with self.subTest(expected=expected):
                client = ScriptedClient(
                    [
                        {"content": [{"type": "text", "text": delta}], "isError": error}
                        for delta in deltas
                    ]
                )
                actual = wait_for_evaluation_output(
                    client, expected, "scripted cell", expected_error=error, **source
                )
                self.assertEqual(actual, expected)
                self.assertEqual(client.calls[0], source)
                for call in client.calls[1:]:
                    self.assertEqual(call.keys(), {"timeout_ms", "session"})
                    self.assertEqual(call["session"], "default")
                result = {
                    "content": [{"type": "text", "text": expected}],
                    "isError": error,
                }
                self.assertEqual(
                    client.transcript, [{"send": source, "result": result}]
                )
                self.assertEqual(client.response_timeout, 600)

    def test_rejects_multipart_without_collapsing_raw_exchange(self) -> None:
        result = {
            "content": [
                {"type": "text", "text": "text"},
                {"type": "image", "data": "bytes"},
            ]
        }
        client = ScriptedClient([result])
        with self.assertRaises(AssertionError):
            wait_for_evaluation_output(client, "text", "multipart", python="once")
        self.assertEqual(len(client.transcript), 1)
        self.assertEqual(len(result["content"]), 2)

    def test_retains_initial_output_cuts_and_exact_error_flags(self) -> None:
        client = ScriptedClient(
            [
                {
                    "content": [
                        {
                            "type": "text",
                            "text": "second\n[running; poll with an empty send]",
                        }
                    ]
                },
                {
                    "content": [{"type": "text", "text": "final error\n"}],
                    "isError": True,
                },
            ]
        )
        cuts = []
        self.assertEqual(
            wait_for_evaluation_output(
                client,
                None,
                "existing evaluation",
                expected_error=None,
                initial_cuts=("first",),
                output_cuts=cuts,
            ),
            "firstsecondfinal error\n",
        )
        self.assertEqual(cuts, ["first", "second", "final error\n"])
        self.assertTrue(client.transcript[-1]["result"]["isError"])
        self.assertTrue(
            all("python" not in call and "r" not in call for call in client.calls)
        )


class MarkerTests(unittest.TestCase):
    def test_cancellation_wakes_an_idle_marker_wait_without_polling(self) -> None:
        with tempfile.TemporaryDirectory() as directory, Events() as events:
            arrived = Event()
            probes = []

            def discover() -> None:
                probes.append(None)
                arrived.set()

            with ThreadPoolExecutor(max_workers=1) as executor:
                pending = executor.submit(
                    wait_for_checkpoint,
                    discover,
                    "cancelled generation",
                    root=Path(directory),
                    events=events,
                    timeout=2,
                )
                try:
                    self.assertTrue(arrived.wait(1))
                    events.cancel()
                    events.cancel()
                    with self.assertRaisesRegex(CancelledError, "cancelled generation"):
                        pending.result(timeout=1)
                    if sys.platform in {"darwin", "linux"}:
                        self.assertEqual(len(probes), 1)
                finally:
                    events.cancel()

    def test_late_nested_marker_and_portable_cancellation(self) -> None:
        for platform in (sys.platform, "win32"):
            with (
                self.subTest(platform=platform),
                patch("support.events.sys.platform", platform),
                tempfile.TemporaryDirectory() as directory,
                Events() as events,
            ):
                root = Path(directory)
                marker = root / "new" / "worker" / "ready"
                arrived = Event()

                def discover() -> Path | None:
                    found = marker if marker.exists() else None
                    arrived.set()
                    return found

                with ThreadPoolExecutor(max_workers=1) as executor:
                    pending = executor.submit(
                        wait_for_checkpoint,
                        discover,
                        "late generation",
                        root=root,
                        recursive=True,
                        events=events,
                        timeout=2,
                    )
                    try:
                        self.assertTrue(arrived.wait(1))
                        marker.parent.mkdir(parents=True)
                        marker.touch()
                        self.assertEqual(pending.result(timeout=1), marker)
                        events.cancel()
                        with self.assertRaises(CancelledError):
                            wait_for_path(
                                marker, "cancel before arrival", events=events
                            )
                    finally:
                        events.cancel()

    def test_marker_wait_watches_a_directory_created_during_subscription(self) -> None:
        with tempfile.TemporaryDirectory() as directory, Events() as events:
            marker = Path(directory) / "late" / "ready"
            subscribe = events.watch_file
            arrived = Event()

            def create_then_subscribe(path: Path) -> None:
                # The directory appears after its ancestor was chosen and
                # before that ancestor can report the creation.
                marker.parent.mkdir(exist_ok=True)
                subscribe(path)

            def discover() -> Path | None:
                found = marker if marker.exists() else None
                arrived.set()
                return found

            with (
                patch.object(events, "watch_file", create_then_subscribe),
                ThreadPoolExecutor(max_workers=1) as executor,
            ):
                pending = executor.submit(
                    wait_for_checkpoint,
                    discover,
                    "late directory",
                    root=marker.parent,
                    events=events,
                    timeout=2,
                )
                try:
                    self.assertTrue(arrived.wait(1))
                    marker.touch()
                    self.assertEqual(pending.result(timeout=1), marker)
                finally:
                    events.cancel()

    def test_exact_generation_path_and_missing_marker_deadline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "old").mkdir()
            (root / "old/ready").touch()
            current = root / "new/ready"
            with self.assertRaisesRegex(TimeoutError, "current generation.*new"):
                wait_for_path(current, "current generation", timeout=0.02)
            current.parent.mkdir()
            current.touch()
            wait_for_path(current, "current generation", timeout=0.02)


@unittest.skipUnless(POSIX.available, POSIX.reason)
class CheckpointTests(unittest.TestCase):
    @unittest.skipUnless(PROCESS_EVENTS.available, PROCESS_EVENTS.reason)
    def test_owner_exit_wakes_pending_marker_wait_with_evidence(self) -> None:
        with (
            tempfile.TemporaryDirectory() as directory,
            Events() as events,
            subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    "import sys; print('ready', flush=True); sys.stdin.read()",
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                text=True,
            ) as owner,
        ):
            self.assertEqual(owner.stdout.readline(), "ready\n")
            client = SimpleNamespace(
                process=owner,
                transcript=[{"result": "last response"}],
                stderr=SimpleNamespace(buffer=b"bounded diagnostic"),
            )
            arrived = Event()

            def discover() -> None:
                arrived.set()

            with ThreadPoolExecutor(max_workers=1) as executor:
                pending = executor.submit(
                    wait_for_checkpoint,
                    discover,
                    "owner exit",
                    root=Path(directory),
                    events=events,
                    client=client,
                    timeout=2,
                )
                try:
                    self.assertTrue(arrived.wait(1))
                    owner.stdin.close()
                    with self.assertRaisesRegex(
                        AssertionError, "owner exit.*last response.*bounded diagnostic"
                    ):
                        pending.result(timeout=1)
                finally:
                    events.cancel()
                    if owner.poll() is None:
                        owner.kill()
                    owner.wait(timeout=2)

    def test_early_release_and_repeated_close_preserve_descriptor_ownership(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with closing(FifoCheckpoint.create(Path(directory) / "gate")) as checkpoint:
                checkpoint.release()
                checkpoint.wait("already released")
                checkpoint.close()
                other = os.open(os.devnull, os.O_RDONLY)
                try:
                    checkpoint.close()
                    os.fstat(other)
                finally:
                    os.close(other)

    def test_rendezvous_requires_a_reader_and_preserves_each_token(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, release, token in (
                ("gate", release_fixture_checkpoint, b"1"),
                (
                    "zod-release-partial-sideband",
                    lambda path, **args: release_partial_sideband(
                        path.with_name("marker"), **args
                    ),
                    b"x",
                ),
            ):
                path = root / name
                os.mkfifo(path)
                with self.assertRaisesRegex(TimeoutError, name):
                    release(path, timeout=0.02)
                with subprocess.Popen(
                    [
                        sys.executable,
                        "-c",
                        "import pathlib,sys; print(pathlib.Path(sys.argv[1]).read_bytes().decode())",
                        str(path),
                    ],
                    stdout=subprocess.PIPE,
                    text=True,
                ) as reader:
                    try:
                        release(path, timeout=2)
                        self.assertEqual(
                            reader.communicate(timeout=2), (token.decode() + "\n", None)
                        )
                        self.assertEqual(reader.returncode, 0)
                    finally:
                        if reader.poll() is None:
                            reader.kill()
                            reader.wait(timeout=2)

    def test_scoped_marker_rejects_stale_generation_and_reports_peer_exit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "old").mkdir()
            (root / "old/ready").touch()
            (root / "new").mkdir()
            with subprocess.Popen([sys.executable, "-c", "pass"]) as owner:
                owner.wait(timeout=2)
                client = SimpleNamespace(
                    process=owner,
                    transcript=[{"result": "last response"}],
                    stderr=SimpleNamespace(buffer=b"bounded diagnostic"),
                )
                with self.assertRaisesRegex(
                    AssertionError, "ready.*last response.*bounded diagnostic"
                ):
                    wait_for_worker_file(root / "new", "ready", client)
                fifo = root / "unread"
                os.mkfifo(fifo)
                with self.assertRaisesRegex(
                    AssertionError, "unread.*bounded diagnostic"
                ):
                    release_fixture_checkpoint(fifo, client=client)


if __name__ == "__main__":
    unittest.main()
