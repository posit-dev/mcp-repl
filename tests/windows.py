"""Native Windows acceptance tests for MCP stdio and Python packaging.

Run with `uv run --no-project tests/windows.py` after `cargo build`.
"""

import ctypes
import base64
import csv
from contextlib import ExitStack
import hashlib
import io
import json
import os
from pathlib import Path
from queue import Queue
import re
import shutil
import subprocess
import sys
import tempfile
import time
from threading import Thread
from textwrap import dedent
import unittest
import zipfile

from windows_gate import Gate
from support.checkpoints import wait_for_path
from support.installation import native_console
from support.normalization import code
from support.r import r_duckdb_snapshot_environment, r_test_environment
from windows_distribution import NATIVE_SUFFIXES, inspect_artifacts

from windows_cargo import WindowsCargo  # noqa: F401 -- include build acceptance
from windows_relay import WindowsRelay  # noqa: F401 -- include protocol acceptance
from windows_resolver import (  # noqa: F401 -- include resolver acceptance
    WindowsResolver,
    WindowsResolverMaterialization,
)
from windows_mxc import WindowsMxc  # noqa: F401 -- include MXC contracts and probing
from windows_preparation import WindowsPreparation  # noqa: F401 -- include resolver enforcement
from windows_sandbox import WindowsSandbox  # noqa: F401 -- include sandbox acceptance


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(
    os.environ.get("MCP_CONSOLE_TEST_BINARY", ROOT / "target/debug/mcp-console.exe")
)
NATIVE_BINARY = native_console(BINARY)


class Session:
    def __init__(
        self,
        environment=None,
        *,
        relay=None,
        python=None,
        bare_r=False,
        defer_bootstrap=False,
        sandbox=False,
        temporary_root=None,
        overrides=(),
        use_r_startup_files=False,
        binary=None,
    ):
        if sandbox:
            from windows_sandbox import workspace

            class Directory:
                def __init__(self):
                    self.context = workspace(temporary_root)
                    self.name = str(self.context.__enter__())

                def cleanup(self):
                    self.context.__exit__(None, None, None)

            self.directory = Directory()
        else:
            self.directory = tempfile.TemporaryDirectory(
                prefix="console windows ", dir=temporary_root
            )
        (Path(self.directory.name) / ".agents/console").mkdir(parents=True)
        self.errors = tempfile.TemporaryFile()
        command = [str(binary or BINARY), "serve"]
        if not sandbox:
            command.append("--no-sandbox")
        for override in overrides:
            command.extend(["-c", override])
        if python is not None:
            command.extend(["-c", f"python={json.dumps(str(python))}"])
        if environment is None:
            environment = dict(os.environ, RETICULATE_PYTHON=sys.executable)
        environment = dict(
            environment, MCP_CONSOLE_HOME=str(Path(self.directory.name) / "home")
        )
        environment.setdefault(
            "MCP_CONSOLE_CACHE_DIR", str(Path(self.directory.name) / "cache")
        )
        if not use_r_startup_files:
            for name in ("R_ENVIRON", "R_ENVIRON_USER", "R_PROFILE", "R_PROFILE_USER"):
                environment[name] = os.devnull
        if bare_r:
            # Hiding ir/uv on PATH is insufficient when ambient reticulate can
            # bootstrap uv. Isolate package libraries for preinstalled-R cases.
            library = Path(self.directory.name) / "r-library"
            library.mkdir()
            for name in ("R_LIBS", "R_LIBS_SITE", "R_LIBS_USER"):
                environment[name] = str(library)
            environment.pop("RETICULATE_UV", None)
        self.defer_bootstrap = defer_bootstrap
        if defer_bootstrap:
            # Interrupt retryable Python setup before eager bootstrap enters R.
            # The test's first cell then chooses which runtime to finish first.
            (Path(self.directory.name) / "sitecustomize.py").write_text(
                dedent("""
                    import builtins
                    import sys

                    if sys.argv[0] == "" and "_mcp_console_services" in sys.modules:
                        if not getattr(builtins, "bootstrap_deferred", False):
                            builtins.bootstrap_deferred = True
                            input("defer bootstrap> ")
                    """)
            )
            environment["RETICULATE_PYTHONPATH"] = self.directory.name
        if relay is not None:
            command.extend(["--worker", str(BINARY), "--relay", str(relay)])
        self.process = subprocess.Popen(
            command,
            cwd=self.directory.name,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.errors,
        )
        self.timeout = 10 if environment and "TEST_RESOLVER_PID" in environment else 180
        self.messages = Queue()
        self.sequence = 0
        Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.process.stdout:
            self.messages.put(json.loads(line))
        self.messages.put(None)

    def request(
        self, method, params, *, close_input=False, deadline: float | None = None
    ):
        if deadline is None:
            deadline = time.monotonic() + self.timeout
        self.sequence += 1
        message = {
            "jsonrpc": "2.0",
            "id": self.sequence,
            "method": method,
            "params": params,
        }
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()
        if close_input:
            self.process.stdin.close()
        while True:
            response = self.messages.get(timeout=max(0, deadline - time.monotonic()))
            if response is None:
                recorded = []
                try:
                    journals = Path(self.directory.name).glob(
                        ".agents/console/sessions/*/capture/events.jsonl"
                    )
                    for journal in journals:
                        for line in journal.read_text(encoding="utf-8").splitlines():
                            event = json.loads(line)
                            if event.get("event") in {"tool_call", "tool_result"}:
                                recorded.append(
                                    {
                                        key: event[key]
                                        for key in ("event", "call_id", "request_id")
                                        if key in event
                                    }
                                )
                except (OSError, ValueError) as error:
                    recorded.append({"diagnostic_error": str(error)})
                self.errors.seek(0)
                errors = self.errors.read().decode(errors="replace")
                raise AssertionError(
                    f"stdout closed before {method} response id {self.sequence}; "
                    f"process status: {self.process.poll()}; "
                    f"stdin closed: {self.process.stdin.closed}; "
                    f"recorded calls: {recorded}\n"
                    f"stderr: {errors or '<empty>'}"
                )
            if response.get("id") == self.sequence:
                assert "error" not in response, response
                return response["result"]

    def initialize(self):
        self.server_info = self.request(
            "initialize",
            {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "windows-acceptance", "version": "1"},
            },
        )
        self.process.stdin.write(
            b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n'
        )
        self.process.stdin.flush()

        if self.defer_bootstrap:
            result = self.send(
                python="raise AssertionError('interrupted bootstrap ran fixture cell')",
                timeout_ms=0,
            )
            # This checkpoint follows cold host dependency preparation, which
            # can exceed an ordinary runtime response allowance on CI.
            deadline = time.monotonic() + 600
            while (
                "waiting for stdin" not in json.dumps(result)
                and time.monotonic() < deadline
            ):
                result = self.send(timeout_ms=1000)
            assert "defer bootstrap> " in json.dumps(result), result
            assert "waiting for stdin" in json.dumps(result), result
            result = self.send(control="interrupt", timeout_ms=10_000)
            assert "KeyboardInterrupt" in json.dumps(result), result
            assert "interrupted bootstrap ran fixture cell" not in json.dumps(result), (
                result
            )
            assert "running;" not in json.dumps(result), result

    def send(self, *, response_deadline: float | None = None, **arguments):
        if response_deadline is None:
            response_deadline = time.monotonic() + self.timeout
        observe_once = "timeout_ms" in arguments
        arguments.setdefault("session", "default")
        arguments.setdefault("timeout_ms", 10000)
        result = self.request(
            "tools/call",
            {"name": "send", "arguments": arguments},
            deadline=response_deadline,
        )
        if observe_once:
            return result
        return self._collect(result, arguments, response_deadline)

    def expect(self, expected: str, **arguments) -> dict:
        """Collect successful output through completion before a dependent call."""
        deadline = time.monotonic() + self.timeout
        result = self.send(**arguments, timeout_ms=0, response_deadline=deadline)
        result = self._collect(result, arguments, deadline, expected=expected)
        assert not result.get("isError"), result
        assert expected in json.dumps(result, ensure_ascii=False), result
        return result

    def observe(self, expected: str, **arguments) -> dict:
        """Observe output from a deliberately running or input-blocked cell."""
        deadline = time.monotonic() + self.timeout
        result = self.send(**arguments, timeout_ms=0, response_deadline=deadline)
        return self._collect(result, arguments, deadline, observation=expected)

    def _collect(
        self,
        result: dict,
        arguments: dict,
        deadline: float,
        *,
        expected: str | None = None,
        observation: str | None = None,
    ) -> dict:
        from support.progress import without_elapsed

        content = []
        running = "\n[running; poll with an empty send]"
        waiting = "[waiting for stdin]"
        controlled = arguments.get("control") is not None and any(
            arguments.get(language) is not None for language in ("r", "python", "sql")
        )
        while True:
            status = ""
            for item in result.get("content", []):
                if item["type"] == "text":
                    text = without_elapsed(item["text"])
                    if text.endswith(running):
                        text = text.removesuffix(running)
                        status = running
                    elif text.endswith(waiting):
                        text = text.removesuffix(waiting)
                        status = waiting
                        if text == "\n":
                            text = ""
                    if not text:
                        continue
                    item = item | {"text": text}
                if (
                    item == {"type": "text", "text": "[done]"}
                    and content
                    and not controlled
                ):
                    continue
                if (
                    item == {"type": "text", "text": "[done]"}
                    and controlled
                    and content
                    and content[-1]["type"] == "text"
                    and not content[-1]["text"].endswith("\n")
                ):
                    item = item | {"text": "\n[done]"}
                if item["type"] == "text" and content and content[-1]["type"] == "text":
                    content[-1] = content[-1] | {
                        "text": content[-1]["text"] + item["text"]
                    }
                else:
                    content.append(item)
            observed_content = content[:]
            if status:
                suffix = status
                if status == waiting and not (
                    observed_content
                    and observed_content[-1]["type"] == "text"
                    and observed_content[-1]["text"].endswith("\n")
                ):
                    suffix = "\n" + status
                if observed_content and observed_content[-1]["type"] == "text":
                    observed_content[-1] = observed_content[-1] | {
                        "text": observed_content[-1]["text"] + suffix
                    }
                else:
                    observed_content.append({"type": "text", "text": suffix})
            collected = result | {"content": observed_content}
            reached = observation is not None and observation in json.dumps(
                collected, ensure_ascii=False
            )
            continuing = not reached and (
                status == running or (expected is not None and status == waiting)
            )
            if not continuing:
                if observation is not None:
                    assert not result.get("isError"), result
                assert observation is None or reached, collected
                return collected
            assert not result.get("isError"), result
            remaining = deadline - time.monotonic()
            assert remaining > 0, (
                f"evaluation did not reach its expected state: {collected}"
            )
            result = self.send(
                session=arguments.get("session", "default"),
                timeout_ms=min(1000, max(1, int(remaining * 1000))),
                response_deadline=deadline,
            )

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
            raise
        finally:
            self.process.stdout.close()
            if sys.exc_info()[0] is not None:
                self.errors.seek(0)
                sys.stderr.write(self.errors.read().decode(errors="replace"))
            self.errors.close()
            self.directory.cleanup()


def exercise_input_and_interrupt(session: Session) -> None:
    """The same public, network-independent scenario for direct execution and Windows MXC."""
    result = session.observe(
        "waiting for stdin",
        # fmt: r
        r=dedent("""
            answer <- readline("Name: ")
            answer
            """),
    )
    assert "waiting for stdin" in json.dumps(result), result
    result = session.expect("Windows", stdin="Windows\n")
    assert "Windows" in json.dumps(result), result
    result = session.observe(
        "Loop ready", r='saved <- 42; cat("Loop ready\\n"); repeat {}'
    )
    assert "running" in json.dumps(result), result
    result = session.send(control="interrupt")
    assert "running;" not in json.dumps(result), result
    result = session.send(r="saved")
    assert "42" in json.dumps(result), result


def exercise_sql_interrupt(session: Session) -> None:
    """Interrupt a SQL callback using the worker's cooperative input boundary."""
    session.expect(
        "SQL callback ready",
        # fmt: python
        python=code("""
            import sqlite3

            interrupt_connection = sqlite3.connect(":memory:")


            def sql_gate():
                input("SQL gate> ")
                return 1


            interrupt_connection.create_function("sql_gate", 0, sql_gate)
            _console.sql_connection(interrupt_connection)
            print("SQL callback ready")
            """),
    )
    session.observe("waiting for stdin", sql="SELECT sql_gate() AS answer")
    result = session.send(control="interrupt", timeout_ms=10000)
    output = json.dumps(result)
    assert not result.get("isError") and "running;" not in output, result
    # SQLite reports its UDF failure after the Python callback is interrupted.
    assert "user-defined function raised exception" in output, result
    session.expect("[done]", python="_console.sql_connection(None)")
    session.expect("1234567", sql="SELECT * FROM retained")


def exercise_r_sql(session: Session) -> None:
    session.request("tools/list", {})
    session.expect("[done]", sql="CREATE TABLE retained AS SELECT 1234567 AS answer")
    session.expect(
        "R connection ready",
        # fmt: r
        r=code("""
            stopifnot(packageVersion("duckdb") == "1.5.6")
            retained_pid <- Sys.getpid()
            managed <- .console$sql_connection()
            stopifnot(DBI::dbGetQuery(managed, "SELECT * FROM retained")$answer == 1234567)
            frame <- data.frame(answer = 7654321L)
            cat("R connection ready")
            """),
    )
    session.expect("7654321", sql="SELECT * FROM frame")
    result = session.send(
        sql="SELECT sum(i::DOUBLE) FROM range(1000000000000) AS values(i)",
        timeout_ms=1000,
    )
    assert not result.get("isError") and "running;" in json.dumps(result), result
    result = session.send(control="interrupt", timeout_ms=10000)
    assert not result.get("isError"), result
    assert result["content"] == [{"type": "text", "text": "\n"}], result
    session.expect("1234567", sql="SELECT * FROM retained")
    session.expect(
        "R worker retained",
        r='stopifnot(Sys.getpid() == retained_pid, identical(.console$sql_connection(), managed)); cat("R worker retained")',
    )
    session.expect("worker stopped", control="restart")
    session.expect("1234567", sql="SELECT 1234567 AS answer")


def exercise_later_callbacks(session: Session) -> None:
    """Observe idle timer input, graphics, interruption, and R/Python state."""
    result = session.send(requirements={"r": ["later"]})
    assert not result.get("isError"), result
    # fmt: r
    r = code(r"""
        callback_answer <- tempfile("later-answer-")
        callback_complete <- tempfile("later-complete-")
        run_callback <- function() {
          if (!file.exists("later-gate")) {
            later::later(run_callback, delay = 0.01)
            return(invisible(NULL))
          }
          idle_answer <<- readline("later> ")
          plot(1:3)
          tryCatch(
            {
              stopifnot(file.create(callback_answer))
              repeat {
                Sys.sleep(1)
              }
            },
            interrupt = function(condition) cat("idle callback interrupted\n")
          )
          stopifnot(file.create(callback_complete))
        }
        later::later(run_callback, delay = 0.01)
        cat(callback_answer, callback_complete, sep = "\n")
        """)
    result = session.send(r=r)
    assert not result.get("isError"), result
    paths = result["content"][0]["text"].splitlines()
    assert len(paths) == 2, result
    answer_path, complete_path = map(Path, paths)
    root = Path(session.directory.name)
    (root / "later-gate").touch()
    deadline = time.monotonic() + 10
    while True:
        result = session.send(timeout_ms=10)
        assert not result.get("isError"), result
        if "waiting for stdin" in json.dumps(result):
            assert "later> " in json.dumps(result), result
            break
        assert time.monotonic() < deadline, result
        time.sleep(0.01)
    answer = session.send(stdin="Windows callback\n")
    try:
        wait_for_path(answer_path, "idle callback received input")
    except TimeoutError as error:
        raise AssertionError(
            [
                item["text"]
                for result in (answer, session.send(timeout_ms=10))
                for item in result.get("content", [])
                if item["type"] == "text"
            ]
        ) from error
    result = session.send(control="interrupt")
    wait_for_path(complete_path, "idle callback caught interrupt")
    content = result["content"][:]
    deadline = time.monotonic() + 10
    while not (
        "idle callback interrupted" in json.dumps(content)
        and any(item["type"] == "image" for item in content)
    ):
        assert not result.get("isError"), result
        assert time.monotonic() < deadline, content
        result = session.send(timeout_ms=10)
        content.extend(result["content"])
        time.sleep(0.01)
    result = session.send(python="print(r.idle_answer)")
    assert not result.get("isError"), result
    assert "Windows callback" in json.dumps(result), result


@unittest.skipUnless(os.name == "nt", "native Windows packaging")
class WindowsPackaging(unittest.TestCase):
    def test_relocated_distribution_preserves_sandbox_startup(self):
        from support.installation import installed_console
        from support.windows_mxc import mxc_requirement

        original = mxc_requirement(BINARY)
        relocated = mxc_requirement(installed_console(NATIVE_BINARY))
        self.assertEqual(relocated.available, original.available)

    def test_native_distribution_has_no_redistributable_vc_runtime(self):
        paths = {NATIVE_BINARY} | {
            path
            for path in (NATIVE_BINARY.parent.parent / "libexec").rglob("*")
            if path.suffix.lower() in NATIVE_SUFFIXES
        }
        inspect_artifacts((path, path.read_bytes()) for path in sorted(paths))

    def test_distribution_audit_checks_normal_and_delayed_imports(self):
        with tempfile.TemporaryDirectory(prefix="console PE imports ") as directory:
            root = Path(directory)
            source = root / "main.rs"
            source.write_text('fn main() { println!("PE import fixture"); }\n')
            for kind in ("normal", "delayed", "static"):
                with self.subTest(imports=kind):
                    binary = root / f"{kind}.exe"
                    build = subprocess.run(
                        [
                            sys.executable,
                            ROOT / "scripts/with-checkout",
                            "rustc",
                            source,
                            "-o",
                            binary,
                            "-C",
                            "target-feature="
                            + ("+" if kind == "static" else "-")
                            + "crt-static",
                            *(
                                [
                                    "-C",
                                    "link-arg=/DELAYLOAD:VCRUNTIME140.dll",
                                    "-C",
                                    "link-arg=delayimp.lib",
                                ]
                                if kind == "delayed"
                                else []
                            ),
                        ],
                        capture_output=True,
                        text=True,
                        timeout=60,
                    )
                    self.assertEqual(build.returncode, 0, build.stderr)
                    wheel = root / f"{kind}.whl"
                    with zipfile.ZipFile(wheel, "w") as archive:
                        archive.write(
                            binary, f"fixture.data/data/libexec/{binary.name}"
                        )
                    audit = subprocess.run(
                        [sys.executable, ROOT / "tests/windows_distribution.py", wheel],
                        capture_output=True,
                        text=True,
                        timeout=30,
                    )
                    if kind == "static":
                        self.assertEqual(audit.returncode, 0, audit.stderr)
                        self.assertIn("kernel32.dll", audit.stdout.lower())
                    else:
                        self.assertNotEqual(audit.returncode, 0)
                        self.assertIn(f"{kind} import VCRUNTIME140.dll", audit.stderr)

    def test_concurrent_build_waits_and_recovers_after_failure(self):
        with ExitStack() as cleanup:
            root = Path(
                cleanup.enter_context(
                    tempfile.TemporaryDirectory(prefix="console packaging ")
                )
            )
            (root / "scripts").mkdir()
            for name in (
                "build_backend.py",
                "checkout_workflow.py",
                "checkout_windows.py",
            ):
                (root / "scripts" / name).write_bytes(
                    (ROOT / "scripts" / name).read_bytes()
                )
            (root / "windows_build.py").write_bytes(
                (ROOT / "tests/fixtures/windows_build.py").read_bytes()
            )

            def start(hook):
                process = subprocess.Popen(
                    [sys.executable, str(root / "windows_build.py"), hook],
                    cwd=root,
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                cleanup.callback(stop, process)
                lines = Queue()

                def read():
                    for line in process.stdout:
                        lines.put(line.strip())
                    lines.put(None)

                Thread(target=read, daemon=True).start()
                self.assertEqual(lines.get(timeout=10), "ready")
                return process, lines

            def stop(process):
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=10)
                process.stdin.close()
                process.stdout.close()
                process.stderr.close()

            first, first_lines = start("build_wheel")
            self.assertEqual(first_lines.get(timeout=10), "building")
            second, second_lines = start("build_editable")
            # The old CRT lock gives up after about ten seconds. A queued
            # build must remain blocked for the full duration of its owner.
            try:
                second.wait(timeout=12)
            except subprocess.TimeoutExpired:
                pass
            else:
                self.fail(f"queued build exited early: {second.stderr.read()}")
            self.assertTrue(second_lines.empty(), "concurrent Maturin builds")
            first.stdin.write("fail\n")
            first.stdin.flush()
            self.assertNotEqual(first.wait(timeout=10), 0)
            self.assertIn("fixture build failed", first.stderr.read())
            self.assertEqual(second_lines.get(timeout=10), "building")
            second.stdin.write("finish\n")
            second.stdin.flush()
            self.assertEqual(second.wait(timeout=10), 0, second.stderr.read())
            self.assertEqual(second_lines.get(timeout=10), "fixture.whl")
            with zipfile.ZipFile(root / "fixture.whl") as wheel:
                self.assertNotIn(
                    "fixture-1.data/scripts/mcp-console.exe", wheel.namelist()
                )
                self.assertEqual(
                    wheel.read("fixture-1.data/data/libexec/mcp-console.exe"),
                    b"native fixture",
                )
                self.assertIn(
                    b"mcp-console = mcp_console._launcher:main",
                    wheel.read("fixture-1.dist-info/entry_points.txt"),
                )
                entry_points = wheel.read("fixture-1.dist-info/entry_points.txt")
                self.assertIn(b"OtherTool = other.module:main", entry_points)
                records = list(
                    csv.reader(
                        io.StringIO(wheel.read("fixture-1.dist-info/RECORD").decode())
                    )
                )
                self.assertEqual({row[0] for row in records}, set(wheel.namelist()))
                for name, digest, size in records:
                    if name.endswith("/RECORD"):
                        self.assertEqual((digest, size), ("", ""))
                    else:
                        content = wheel.read(name)
                        expected = (
                            base64.urlsafe_b64encode(hashlib.sha256(content).digest())
                            .rstrip(b"=")
                            .decode()
                        )
                        self.assertEqual(digest, "sha256=" + expected)
                        self.assertEqual(int(size), len(content))
            archive, archive_lines = start("build_sdist")
            self.assertEqual(archive_lines.get(timeout=10), "building")
            archive.stdin.write("finish\n")
            archive.stdin.flush()
            self.assertEqual(archive.wait(timeout=10), 0, archive.stderr.read())
            self.assertEqual(archive_lines.get(timeout=10), "fixture.whl")
            metadata, metadata_lines = start("prepare_metadata_for_build_wheel")
            self.assertEqual(metadata_lines.get(timeout=10), "building")
            metadata.stdin.write("finish\n")
            metadata.stdin.flush()
            self.assertEqual(metadata.wait(timeout=10), 0, metadata.stderr.read())
            self.assertEqual(metadata_lines.get(timeout=10), "fixture-1.dist-info")
            self.assertEqual(
                (root / "fixture-1.dist-info/entry_points.txt").read_bytes(),
                entry_points,
            )


@unittest.skipUnless(os.name == "nt", "native Windows acceptance")
class WindowsConsole(unittest.TestCase):
    def test_collects_streamed_output_before_starting_the_next_cell(self):
        from concurrent.futures import ThreadPoolExecutor

        from support.checkpoints import wait_for_checkpoint
        from support.previews import live_recording_events

        session = self.session()
        session.send(python="pass")
        (recording,) = (
            Path(session.directory.name) / ".agents/console/sessions"
        ).iterdir()
        gate = Gate(timeout=session.timeout)
        self.addCleanup(gate.close)

        def release_after_running_result():
            gate.accept(session.process)
            self.assertEqual(gate.readline(), b"ready\n")

            def running_result():
                for event in live_recording_events(recording):
                    if event["event"] != "tool_result":
                        continue
                    text = "".join(
                        item.get("text", "") for item in event["result"]["content"]
                    )
                    if "stream prefix" in text and text.endswith(
                        "[running; poll with an empty send]"
                    ):
                        return recording
                return None

            # Release only after a public running response contains the prefix.
            # This forces two observations without depending on evaluator speed.
            wait_for_checkpoint(
                running_result,
                "stream prefix was returned while the cell was gated",
                root=recording / "capture",
                timeout=session.timeout,
            )
            gate.sendall(b"1")

        with ThreadPoolExecutor(max_workers=1) as executor:
            released = executor.submit(release_after_running_result)
            result = session.expect(
                "stream prefix",
                control="restart",
                # fmt: python
                python=code(f"""
                    with open({gate.name!r}, "r+b", buffering=0) as gate:
                        gate.write(b"ready\\n")
                        print("stream prefix")
                        assert gate.read(1) == b"1"
                    print("stream suffix", end="")
                    streamed_state = 42
                    """),
            )
            released.result(timeout=session.timeout)
        self.assertEqual(
            result["content"],
            [
                {
                    "type": "text",
                    "text": "[worker stopped: in-memory state lost]\n"
                    "[starting new worker]\nstream prefix\nstream suffix\n[done]",
                }
            ],
        )
        result = session.send(python="streamed_state")
        self.assertEqual(result["content"], [{"type": "text", "text": "42\n"}])

    def test_completed_recording_in_inherited_acl_workspace(self):
        from support.client import McpClient
        from support.execution import DIRECT
        from windows_sandbox import workspace

        with workspace() as root:
            with McpClient(
                BINARY,
                DIRECT.serve(
                    "-c", "r=null", "-c", "python=" + json.dumps(sys.executable)
                ),
                os.environ.copy(),
                root,
                # An installed command is a launcher, so the server's recording
                # is not named after this process. The case reads the recording
                # only after the server exits.
                recording_receipts=False,
            ) as client:
                client.initialize_and_list_tools()
                client.expect("42\n", python="print(42)")
                client.finish()
            (recording,) = (root / ".agents/console/sessions").iterdir()
            self.assertEqual(
                (recording / "capture/sessions/001/cell-001.txt").read_text(), "42\n"
            )
            for path in recording.rglob("*"):
                if path.is_file():
                    self.assertFalse(path.stat().st_mode & 0o222, path)

    def test_tilde_paths_use_profile_and_explicit_home(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            profile = root / "profile"
            explicit = root / "explicit-home"
            for home in (profile, explicit):
                home.mkdir()
                shutil.copy2(sys.executable, home / "python.exe")
                (home / "config.yaml").write_text(
                    json.dumps(
                        {
                            "r": "~\\R\\bin\\R.exe",
                            "python": "~\\python.exe",
                            "sandbox": {"filesystem": {"deny": ["~\\private"]}},
                        }
                    ),
                    encoding="utf-8",
                )
            environment = os.environ | {
                "USERPROFILE": str(profile),
                "R_USER": str(root / "r-user"),
                "MCP_CONSOLE_HOME": str(root / "console"),
            }
            environment.pop("HOME", None)
            for value in (None, "", str(explicit)):
                with self.subTest(HOME=value):
                    env = environment.copy()
                    if value is not None:
                        env["HOME"] = value
                    result = subprocess.run(
                        [
                            BINARY,
                            "config",
                            "resolve",
                            "--format",
                            "json",
                            "--config-file",
                            "~\\config.yaml",
                        ],
                        cwd=root,
                        env=env,
                        capture_output=True,
                        text=True,
                        timeout=10,
                    )
                    self.assertEqual(result.returncode, 0, result)
                    self.assertEqual(result.stderr, "", result)
                    settings = json.loads(result.stdout)
                    home = explicit if value else profile
                    self.assertEqual(
                        settings["r"], {"executable": str(home / "R/bin/R.exe")}
                    )
                    self.assertEqual(
                        settings["python"], {"existing": str(home / "python.exe")}
                    )
                    self.assertEqual(
                        settings["sandbox"]["filesystem"]["deny"],
                        [str(home / "private")],
                    )

    def test_native_home_configuration_discovery(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            workspace = root / "workspace"
            workspace.mkdir()
            profile = root / "profile"
            config = profile / ".agents/console/config.yaml"
            config.parent.mkdir(parents=True)
            config.write_text("languages: []", encoding="utf-8")
            environment = os.environ | {"USERPROFILE": str(profile)}
            for name in ("MCP_CONSOLE_HOME", "HOME"):
                environment.pop(name, None)

            def launch(env):
                result = subprocess.run(
                    [BINARY, "serve", "--no-sandbox"],
                    cwd=workspace,
                    env=env,
                    input="",
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 1, result)
                self.assertEqual(result.stdout, "", result)
                return result.stderr

            for home in (None, ""):
                with self.subTest(HOME=home):
                    env = environment.copy()
                    if home is not None:
                        env["HOME"] = home
                    error = launch(env)
                    self.assertIn(str(config), error)
                    self.assertIn("languages must contain at least one", error)
            explicit_home = root / "explicit-home"
            explicit_config = explicit_home / ".agents/console/config.yaml"
            explicit_config.parent.mkdir(parents=True)
            explicit_config.write_text("cache: invalid", encoding="utf-8")
            error = launch(environment | {"HOME": str(explicit_home)})
            self.assertIn(str(explicit_config), error)
            self.assertNotIn(str(config), error)
            for name in ("HOME", "USERPROFILE"):
                with self.subTest(relative=name):
                    self.assertIn(
                        "must be an absolute path",
                        launch(environment | {name: "relative"}),
                    )

    def test_consumes_idle_interrupt_before_next_cell(self):
        # Exercise the same public contract as the Unix Python-only case,
        # including both runtime initialization orders and a shared send.
        for order in ("python", "python-r", "r-python"):
            with self.subTest(order=order):
                session = Session(defer_bootstrap=True)
                try:
                    session.initialize()
                    if order == "r-python":
                        self.assertIn("42", json.dumps(session.send(r="42L")))
                    self.assertFalse(
                        session.send(python="import time; retained = 0")["isError"]
                    )
                    if order == "python-r":
                        self.assertIn("42", json.dumps(session.send(r="42L")))
                    for expected in range(1, 41):
                        control = {"control": "interrupt"}
                        if expected % 2:
                            result = session.send(**control)
                            if order != "python":
                                # Empty polls do not dispatch worker commands.
                                # R must acknowledge while idle, even if the
                                # interrupt call's grace ended before it ran.
                                deadline = time.monotonic() + 5
                                while (
                                    result["content"]
                                    == [{"type": "text", "text": "\n[idle]"}]
                                    and time.monotonic() < deadline
                                ):
                                    time.sleep(0.01)
                                    result = session.send()
                                self.assertEqual(
                                    result["content"],
                                    [{"type": "text", "text": "\n\n[idle]"}],
                                )
                            control = {}
                        result = session.send(
                            **control,
                            python="time.sleep(0.01); retained += 1; print(retained)",
                        )
                        output = "".join(item["text"] for item in result["content"])
                        self.assertFalse(result["isError"], result)
                        wanted = f"{expected}\n" + ("[done]" if control else "")
                        if control and order != "python":
                            # R's acknowledgment may reach the server before
                            # or after it admits the same-call following cell.
                            self.assertIn(
                                output,
                                (
                                    "\n" + wanted,
                                    "\n[output produced while idle]\n" + wanted,
                                ),
                            )
                        else:
                            self.assertEqual(output, wanted)
                    if order != "python":
                        result = session.send(control="interrupt", r="42L")
                        self.assertIn("[1] 42", json.dumps(result))
                        self.assertFalse(result["isError"], result)
                finally:
                    session.close()

    def test_interrupt_and_following_cell_keep_input_and_state_separate(self):
        session = self.session(defer_bootstrap=True)
        session.send(python="history = []")
        for initialize_r in (False, True):
            if initialize_r:
                session.send(r="42L")
            result = session.send(
                python="history.append('old'); input('Old input: '); history.append('wrong')"
            )
            self.assertIn("waiting for stdin", json.dumps(result))
            result = session.send(
                control="interrupt",
                python="history.append('new'); value = input('New input: '); history.append(value)",
            )
            output = json.dumps(result)
            self.assertIn("KeyboardInterrupt", output)
            self.assertIn("New input: ", output)
            self.assertIn("waiting for stdin", output)
            # Queuing stdin does not confirm consumption or cell completion.
            result = session.expect("[done]", stdin="delivered to new cell\n")
            self.assertNotIn("waiting for stdin", json.dumps(result))
            expected = ["old", "new", "delivered to new cell"] * (1 + initialize_r)
            result = session.send(python="print(history)")
            self.assertEqual(
                result["content"], [{"type": "text", "text": repr(expected) + "\n"}]
            )

    def test_rejects_startup_environment_mutation(self):
        with tempfile.TemporaryDirectory(prefix="console startup ") as directory:
            (Path(directory) / "sitecustomize.py").write_text(
                "import sys; sys.prefix = 'changed-by-startup-hook'\n"
            )
            session = Session(
                dict(
                    os.environ,
                    RETICULATE_PYTHON=sys.executable,
                    RETICULATE_PYTHONPATH=directory,
                )
            )
            try:
                session.initialize()
                result = session.send(
                    python="raise AssertionError('invalid environment ran code')"
                )
                output = json.dumps(result)
                self.assertTrue(result["isError"], result)
                self.assertIn(
                    "RuntimeError: embedded Python prefix differs from the selected environment",
                    output,
                )
                self.assertNotIn("FileNotFoundError", output)
                self.assertNotIn("invalid environment ran code", output)
            finally:
                session.close()

    def test_empty_python_selection_uses_available_runtimes(self):
        uv = shutil.which("uv")
        self.assertIsNotNone(uv, "Windows resolver acceptance requires uv")
        r_home = (
            os.environ.get("R_HOME")
            or subprocess.check_output(
                [shutil.which("R") or "R", "RHOME"], text=True
            ).strip()
        )
        system_path = str(Path(os.environ["SystemRoot"]) / "System32")
        for with_python in (False, True):
            with self.subTest(with_python=with_python):
                environment = dict(
                    os.environ,
                    R_HOME=r_home,
                    RETICULATE_PYTHON="",
                    PATH=(
                        str(Path(sys.executable).parent) + os.pathsep
                        if with_python
                        else ""
                    )
                    + system_path,
                )
                if with_python:
                    # The managed pair needs a resolver even with a restricted
                    # PATH; do not depend on reticulate's ambient uv cache.
                    environment.update(
                        RETICULATE_UV=uv,
                        # uv-installed acceptance interpreters are also valid;
                        # downloads stay disabled so discovery uses local installs.
                        UV_PYTHON_PREFERENCE="system",
                        UV_PYTHON_DOWNLOADS="never",
                    )
                session = Session(environment, bare_r=not with_python)
                try:
                    session.initialize()
                    self.assertIn("42", json.dumps(session.send(r="42L")))
                    if with_python:
                        self.assertIn("43", json.dumps(session.send(python="43")))
                finally:
                    session.close()

    def test_eof_preserves_first_send_response(self):
        # A custom worker keeps this independent of runtime preparation, which
        # EOF may cancel. The malformed call never needs to launch the worker.
        session = Session(relay=BINARY)
        self.addCleanup(session.close)
        session.initialize()
        session.request("tools/list", {})
        # Span multiple input chunks so physical EOF can arrive while the
        # asynchronous transport is still collecting the complete request.
        result = session.request(
            "tools/call",
            {"name": "send", "arguments": {"r": "1" + " " * 131072, "python": "1"}},
            close_input=True,
        )
        self.assertEqual(
            result,
            {
                "content": [
                    {
                        "type": "text",
                        "text": "only one of `r`, `python`, or `sql` may be supplied",
                    }
                ],
                "isError": True,
            },
        )
        self.assertEqual(session.process.wait(timeout=10), 0)

    def test_explicit_r_selections_use_windows_installation(self):
        selected = shutil.which("R")
        self.assertIsNotNone(selected, "Windows R selection acceptance requires R")
        home = Path(subprocess.check_output([selected, "RHOME"], text=True).strip())
        self.assertTrue((home / "etc/Rcmd_environ").is_file())
        for selection in (selected, {"executable": selected}):
            with self.subTest(selection=selection):
                session = Session(overrides=["r=" + json.dumps(selection)])
                try:
                    session.initialize()
                    self.assertIn(
                        "selected R ready",
                        json.dumps(session.send(r='cat("selected R ready")')),
                    )
                    self.assertIn(
                        "selected R retained",
                        json.dumps(
                            session.send(
                                control="restart", r='cat("selected R retained")'
                            )
                        ),
                    )
                finally:
                    session.close()

    def test_r_without_python(self):
        # Capture R before removing the interpreter launchers from PATH.
        r_home = (
            os.environ.get("R_HOME")
            or subprocess.check_output(
                [shutil.which("R") or "R", "RHOME"], text=True
            ).strip()
        )
        environment = dict(
            os.environ,
            R_HOME=r_home,
            PATH=str(Path(os.environ["SystemRoot"]) / "System32"),
        )
        environment.pop("RETICULATE_PYTHON", None)
        session = Session(environment, bare_r=True)
        self.addCleanup(session.close)
        session.initialize()
        self.assertIn("42", json.dumps(session.send(r="answer <- 42L; answer")))
        # A running response can precede evaluator entry; observe cell output.
        self.assertIn(
            "running;",
            json.dumps(
                session.observe(
                    "R sleep entered", r='cat("R sleep entered\\n"); Sys.sleep(60)'
                )
            ),
        )
        self.assertNotIn(
            "running;", json.dumps(session.send(control="interrupt", timeout_ms=2000))
        )
        self.assertIn("42", json.dumps(session.send(r="answer")))
        self.assertIn(
            "FALSE", json.dumps(session.send(control="restart", r="exists('answer')"))
        )

    def test_discovers_r_from_batch_launcher(self):
        r_home = Path(
            os.environ.get("R_HOME")
            or subprocess.check_output(
                [shutil.which("R") or "R", "RHOME"], text=True
            ).strip()
        )
        for extension in ("bat", "cmd"):
            with (
                self.subTest(extension=extension),
                tempfile.TemporaryDirectory(prefix="console R launcher ") as directory,
            ):
                root = Path(directory)
                launcher = root / f"R.{extension}"
                launcher.write_text(f'@"{r_home / "bin/R.exe"}" %*\n')
                later = root / "later"
                later.mkdir()
                # An earlier batch launcher takes precedence over a later exe.
                (later / "R.exe").write_text("broken later installation")
                environment = dict(
                    os.environ,
                    PATH=os.pathsep.join(
                        (
                            str(root),
                            str(later),
                            str(Path(os.environ["SystemRoot"]) / "System32"),
                        )
                    ),
                    RETICULATE_PYTHON=sys.executable,
                )
                environment.pop("R_HOME", None)
                session = Session(environment, bare_r=True)
                try:
                    session.initialize()
                    result = session.send(r="answer <- 42L; answer")
                    self.assertFalse(result.get("isError"), result)
                    self.assertIn("42", json.dumps(result))
                    # The selected installation survives changes to the launcher.
                    launcher.write_text("@exit /b 91\n")
                    result = session.send(control="restart", r="exists('answer')")
                    self.assertFalse(result.get("isError"), result)
                    self.assertIn("FALSE", json.dumps(result))
                finally:
                    session.close()

    def test_reports_broken_r_batch_launcher(self):
        with tempfile.TemporaryDirectory(prefix="console broken R ") as directory:
            root = Path(directory)
            (root / "R.bat").write_text(
                "@echo deliberate R discovery failure 1>&2\n@exit /b 91\n"
            )
            environment = dict(
                os.environ,
                PATH=os.pathsep.join(
                    (str(root), str(Path(os.environ["SystemRoot"]) / "System32"))
                ),
                RETICULATE_PYTHON=sys.executable,
            )
            environment.pop("R_HOME", None)
            session = Session(environment)
            try:
                session.initialize()
                result = session.send(r="42L")
                self.assertTrue(result.get("isError"), result)
                self.assertIn("worker R home discovery failed", json.dumps(result))
                self.assertIn("deliberate R discovery failure", json.dumps(result))
            finally:
                session.close()

    def test_python_sleep_interrupt(self):
        for order in ("python", "python-r", "r-python"):
            with self.subTest(order=order):
                session = Session(defer_bootstrap=True)
                try:
                    session.initialize()
                    if order == "r-python":
                        self.assertIn("42", json.dumps(session.send(r="42")))
                    session.send(python="import time; saved = 42")
                    if order == "python-r":
                        self.assertIn("42", json.dumps(session.send(r="42")))
                    # Wait for execution rather than a short observation timeout.
                    self.assertIn(
                        "running;",
                        json.dumps(
                            session.observe(
                                "Python sleep entered",
                                python="print('Python sleep entered'); time.sleep(60)",
                            )
                        ),
                    )
                    result = session.send(control="interrupt", timeout_ms=2000)
                    self.assertIn("KeyboardInterrupt", json.dumps(result))
                    self.assertNotIn("running;", json.dumps(result))
                    self.assertIn("42", json.dumps(session.send(python="saved")))
                finally:
                    session.close()

    def test_interrupt_then_input_after_r_attachment(self):
        session = self.session(defer_bootstrap=True)
        self.assertIn("42", json.dumps(session.send(python="saved = 42; saved")))
        self.assertIn(
            "running;",
            json.dumps(
                session.observe(
                    "Python loop entered",
                    python="print('Python loop entered')\nwhile True: pass",
                )
            ),
        )
        self.assertIn(
            "KeyboardInterrupt",
            json.dumps(session.send(control="interrupt", timeout_ms=2000)),
        )
        self.assertIn("42", json.dumps(session.send(r="42L")))
        # A failed iteration leaves an active cell: stop at the first failure.
        for _ in range(12):
            result = session.observe(
                "R loop entered",
                r='cat("R loop entered\\n"); repeat { Sys.sleep(0) }',
            )
            self.assertIn("running;", json.dumps(result))
            result = session.send(control="interrupt", timeout_ms=2000)
            self.assertNotIn("running;", json.dumps(result))
            for arguments in (
                {"r": "readline('R input: ')"},
                {"python": "input('Python input: ')"},
            ):
                result = session.observe("waiting for stdin", **arguments)
                self.assertIn("waiting for stdin", json.dumps(result))
                result = session.send(control="interrupt", timeout_ms=2000)
                self.assertNotIn("running;", json.dumps(result))
                self.assertNotIn("waiting for stdin", json.dumps(result))
            self.assertIn("42", json.dumps(session.send(python="saved")))

    def test_python_without_r(self):
        environment = dict(
            os.environ,
            PATH=str(Path(os.environ["SystemRoot"]) / "System32"),
            RETICULATE_PYTHON=sys.executable,
        )
        environment.pop("R_HOME", None)
        session = Session(environment)
        self.addCleanup(session.close)
        session.initialize()
        # fmt: python
        result = session.send(
            python=dedent("""
                import ctypes
                from pathlib import Path
                import sys

                assert ctypes.windll.kernel32.GetModuleHandleW("R.dll") == 0
                # Console must find Python's supplied runtime without relying
                # on a separately installed system redistributable. A Python
                # build with its own static CRT may not load this DLL at all.
                supplied = Path(sys.base_prefix) / "vcruntime140.dll"
                if supplied.is_file():
                    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
                    kernel.GetModuleHandleW.argtypes = [ctypes.c_wchar_p]
                    kernel.GetModuleHandleW.restype = ctypes.c_void_p
                    kernel.GetModuleFileNameW.argtypes = [
                        ctypes.c_void_p,
                        ctypes.c_wchar_p,
                        ctypes.c_ulong,
                    ]
                    kernel.GetModuleFileNameW.restype = ctypes.c_ulong
                    module = kernel.GetModuleHandleW("vcruntime140.dll")
                    if module:
                        loaded = ctypes.create_unicode_buffer(32768)
                        assert kernel.GetModuleFileNameW(module, loaded, len(loaded))
                        assert supplied.samefile(loaded.value), (str(supplied), loaded.value)
                answer = 42
                print("Python alone:", answer)
                """)
        )
        self.assertIn("Python alone: 42", json.dumps(result))
        self.assertFalse(result.get("isError"), result)
        result = session.observe(
            "waiting for stdin", python="name = input('Name: '); print(name)"
        )
        self.assertIn("waiting for stdin", json.dumps(result))
        self.assertIn(
            "caf\u00e9",
            json.dumps(
                session.expect("caf\u00e9", stdin="caf\u00e9\n"), ensure_ascii=False
            ),
        )
        result = session.observe(
            "Python loop entered",
            python="print('Python loop entered')\nwhile True: pass",
        )
        self.assertIn("running;", json.dumps(result))
        result = session.send(control="interrupt", timeout_ms=2000)
        self.assertIn("KeyboardInterrupt", json.dumps(result))
        self.assertNotIn("running;", json.dumps(result))
        self.assertIn("42", json.dumps(session.send(python="answer")))
        result = session.send(r="42")
        self.assertTrue(result.get("isError"), result)
        self.assertIn("R cells are unavailable", json.dumps(result))
        result = session.send(
            control="restart", python="print('fresh', 'answer' in globals())"
        )
        self.assertIn("fresh False", json.dumps(result))

    def test_r_initializes_before_python(self):
        session = self.session(defer_bootstrap=True)
        # fmt: r
        result = session.send(
            r=dedent("""
                r_value <- 41L
                stopifnot(!reticulate::py_available(initialize = FALSE))
                cat("R without Python")
                """)
        )
        self.assertIn("R without Python", json.dumps(result))
        result = session.send(python="python_value = 42; print(r.r_value)")
        self.assertIn("41", json.dumps(result))
        self.assertNotIn("Traceback", json.dumps(result))
        result = session.send(
            r="stopifnot(py$python_value == 42L); cat('bridge ready')"
        )
        self.assertIn("bridge ready", json.dumps(result))

    def test_direct_py_access_attaches_on_demand(self):
        for getter in ("reticulate::py", "py"):
            with self.subTest(getter=getter):
                session = self.session()
                for restart in (False, True):
                    if restart:
                        session.send(control="restart")
                    # fmt: python
                    session.send(
                        python=dedent("""
                            bridge_value = "startup value"
                            bridge_object = object()
                            bridge_identity = id(bridge_object)
                            """)
                    )
                    # fmt: r
                    result = session.send(
                        r=dedent("""
                            stopifnot(!reticulate::py_available(initialize = FALSE))
                            suppressPackageStartupMessages(library(reticulate))
                            stopifnot(!reticulate::py_available(initialize = FALSE))
                            main <- GETTER
                            stopifnot(!reticulate::py_available(initialize = FALSE))
                            stopifnot(
                              identical(main$bridge_value, "startup value"),
                              identical(GETTER$bridge_value, "startup value"),
                              reticulate::py_available(initialize = FALSE)
                            )
                            bridge_from_r <- 42L
                            cat("direct bridge ready")
                            """).replace("GETTER", getter)
                    )
                    self.assertIn("direct bridge ready", json.dumps(result))
                    # fmt: python
                    result = session.send(
                        python=dedent("""
                            assert id(bridge_object) == bridge_identity
                            assert bridge_value == "startup value"
                            assert int(r.bridge_from_r) == 42
                            print("bridge state retained")
                            """)
                    )
                    self.assertIn("bridge state retained", json.dumps(result))

    def test_loading_reticulate_does_not_start_python(self):
        session = Session(dict(os.environ, MCP_CONSOLE_LANGUAGES="r"))
        self.addCleanup(session.close)
        session.initialize()
        # fmt: r
        result = session.send(
            r=dedent("""
                Sys.setenv(RETICULATE_PYTHON = "__console_missing_python__")
                suppressPackageStartupMessages(library(reticulate))
                stopifnot(!reticulate::py_available(initialize = FALSE))
                cat("reticulate loaded without Python")
                """)
        )
        self.assertIn("reticulate loaded without Python", json.dumps(result))

    def test_py_reads_in_initialization_hooks_do_not_reenter(self):
        for clear_callback in (False, True):
            with self.subTest(clear_callback=clear_callback):
                session = self.session()
                session.send(python='hook_value = "existing Python value"')
                # fmt: r
                result = session.send(
                    r=dedent("""
                        callback_calls <- 0L
                        after_calls <- 0L
                        options(reticulate.python.beforeInitialized = function() {
                          callback_calls <<- callback_calls + 1L
                          if (callback_calls > 1L) {
                            stop("recursive bridge initialization")
                          }
                          if (CLEAR_CALLBACK) {
                            options(reticulate.python.beforeInitialized = NULL)
                          }
                          stopifnot(
                            is.null(reticulate::py),
                            is.null(py$hook_value),
                            !reticulate::py_available(initialize = FALSE)
                          )
                        })
                        options(reticulate.python.afterInitialized = function() {
                          after_calls <<- after_calls + 1L
                          stopifnot(identical(py$hook_value, "existing Python value"))
                        })
                        stopifnot(
                          identical(reticulate::py$hook_value, "existing Python value"),
                          callback_calls == 1L,
                          after_calls == 1L
                        )
                        options(reticulate.python.beforeInitialized = NULL)
                        options(reticulate.python.afterInitialized = NULL)
                        cat("initialization callback ran once")
                        """).replace(
                        "CLEAR_CALLBACK", "TRUE" if clear_callback else "FALSE"
                    )
                )
                self.assertIn("initialization callback ran once", json.dumps(result))
                self.assertIn(
                    "existing Python value",
                    json.dumps(session.send(python="hook_value")),
                )

    def test_selected_virtualenv_with_unicode_path(self):
        directory = tempfile.TemporaryDirectory(prefix="console Python \u03bb ")
        self.addCleanup(directory.cleanup)
        subprocess.run(
            [sys.executable, "-m", "venv", "--without-pip", directory.name], check=True
        )
        executable = Path(directory.name) / "Scripts/python.exe"
        session = Session(python=executable)
        self.addCleanup(session.close)
        session.initialize()
        # fmt: python
        source = dedent("""
            import os, sys, subprocess, json

            assert sys.prefix != sys.base_prefix
            child = json.loads(
                subprocess.check_output(
                    [sys.executable, "-c", "import sys, json; print(json.dumps(sys.prefix))"],
                    text=True,
                )
            )
            assert os.path.samefile(sys.prefix, child)
            print("virtualenv retained")
            """)
        self.assertIn("virtualenv retained", json.dumps(session.send(python=source)))
        self.assertIn(
            "virtualenv retained",
            json.dumps(session.send(control="restart", python=source)),
        )

    def test_configured_language_presentation_matrix(self):
        script_guidance = (
            "For a reusable R script, include imports, data inputs, and "
            "`#| packages:`/`#| r-version:` metadata for `ir run script.R`, "
            "which starts without live Console objects."
        )
        for custom in (False, True):
            for languages in (
                "r,python,sql",
                "r,python",
                "r",
                "python",
                "sql",
                "r,sql",
                "python,sql",
            ):
                with self.subTest(custom=custom, languages=languages):
                    environment = dict(
                        os.environ,
                        PATH=str(Path(os.environ["SystemRoot"]) / "System32"),
                        RETICULATE_PYTHON=sys.executable,
                        MCP_CONSOLE_LANGUAGES=languages,
                    )
                    environment.pop("R_HOME", None)
                    session = Session(
                        environment, relay=sys.executable if custom else None
                    )
                    try:
                        session.initialize()
                        tool = session.request("tools/list", {})["tools"][0]
                        instructions = session.server_info["instructions"]
                        self.assertEqual(
                            instructions, tool["description"].split("\n\n", 1)[0]
                        )
                        self.assertNotIn("\n", instructions)
                        self.assertLessEqual(len(instructions), 250)
                        self.assertLessEqual(len(tool["description"]), 2048)
                        self.assertEqual(session.request("ping", {}), {})
                        properties = tool["inputSchema"]["properties"]
                        fields = set(languages.split(","))
                        names = {
                            name
                            for name in ("R", "Python", "SQL")
                            if name.lower() in fields
                        }
                        self.assertEqual(
                            set(re.findall(r"\b(?:R|Python|SQL)\b", instructions)),
                            names,
                        )
                        self.assertEqual(
                            set(
                                re.findall(
                                    r"\b(?:R|Python|SQL)\b",
                                    json.dumps(tool["inputSchema"]),
                                )
                            ),
                            names,
                        )
                        self.assertEqual(
                            set(properties) & {"r", "python", "sql"}, fields
                        )
                        self.assertIn(
                            "cooperative interrupt",
                            properties["control"]["description"],
                        )
                        self.assertNotIn("SIGINT", properties["control"]["description"])
                        self.assertEqual(
                            script_guidance
                            in properties.get("r", {}).get("description", ""),
                            not custom and "r" in fields,
                        )
                        if custom:
                            self.assertIn(
                                "Persistent custom-worker",
                                tool["description"],
                            )
                            self.assertEqual(
                                "Switch languages when useful" in tool["description"],
                                len(fields) > 1,
                            )
                        else:
                            self.assertIn(
                                "Persistent "
                                + (
                                    "R, Python, and SQL"
                                    if fields == {"r", "python", "sql"}
                                    else " and ".join(
                                        name
                                        for name in ("R", "Python", "SQL")
                                        if name.lower() in fields
                                    )
                                )
                                + " REPL",
                                tool["description"],
                            )
                            self.assertEqual(
                                "consider DuckDB SQL first" in tool["description"],
                                "sql" in fields and not {"r", "python"} <= fields,
                            )
                            self.assertEqual(
                                "Choose whichever language makes each step"
                                in tool["description"],
                                len(fields) > 1,
                            )
                        self.assertEqual(
                            session.request("tools/list", {})["tools"], [tool]
                        )
                        configured = Session(
                            environment,
                            relay=sys.executable if custom else None,
                            overrides=(
                                "languages=" + json.dumps(languages.split(",")),
                            ),
                        )
                        try:
                            configured.initialize()
                            self.assertEqual(
                                configured.request("tools/list", {})["tools"], [tool]
                            )
                            self.assertEqual(
                                configured.server_info["instructions"], instructions
                            )
                        finally:
                            configured.close()
                    finally:
                        session.close()

    def test_selected_python_rejects_managed_requirements(self):
        environment = dict(
            os.environ,
            PATH=str(Path(os.environ["SystemRoot"]) / "System32"),
            RETICULATE_PYTHON=sys.executable,
        )
        environment.pop("R_HOME", None)
        session = Session(environment)
        self.addCleanup(session.close)
        session.initialize()
        schema = session.request("tools/list", {})
        tool = schema["tools"][0]
        properties = tool["inputSchema"]["properties"]
        self.assertIn("sql", properties)
        self.assertIn(
            "Requires host preparation support",
            properties["requirements"]["description"],
        )
        self.assertTrue(
            tool["description"].startswith("Persistent R, Python, and SQL REPL for ")
        )
        for language in ("r", "python"):
            with self.subTest(language=language):
                description = properties[language]["description"].lower()
                self.assertIn("sql", description)
        self.assertIn(
            "managed sessions prepare missing", properties["r"]["description"]
        )
        self.assertIn(
            "Use installed packages in the selected environment; `requirements.python` is unavailable.",
            properties["python"]["description"],
        )
        self.assertEqual(
            schema["tools"][0]["inputSchema"]["properties"]["requirements"][
                "properties"
            ]["action"]["enum"],
            ["get", "add", "set", "reset"],
        )
        self.assertNotIn("Language fields describe", tool["description"])
        self.assertIn(
            "explicitly selected Python environments",
            properties["requirements"]["properties"]["python"]["description"],
        )
        control = properties["control"]["description"]
        self.assertIn("cooperative interrupt", control)
        self.assertNotIn("SIGINT", control)
        result = session.send(requirements={"action": "add", "python": ["six"]})
        self.assertTrue(result.get("isError"), result)
        self.assertIn("unavailable", json.dumps(result))
        self.assertIn("42", json.dumps(session.send(python="42")))

    def test_sql_without_r(self):
        uv = shutil.which("uv")
        self.assertIsNotNone(uv, "Windows SQL acceptance requires uv")
        directory = tempfile.TemporaryDirectory(prefix="console SQL 日本語 ")
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        shutil.copyfile(uv, root / "uv.exe")
        environment = dict(
            os.environ,
            PATH=os.pathsep.join(
                [
                    str(root),
                    str(Path(sys.executable).parent),
                    str(Path(os.environ["SystemRoot"]) / "System32"),
                ]
            ),
            RETICULATE_PYTHON="managed",
            UV_PYTHON_PREFERENCE="system",
            UV_PYTHON_DOWNLOADS="never",
        )
        environment.pop("R_HOME", None)
        session = Session(environment, overrides=('languages=["python","sql"]',))
        self.addCleanup(session.close)
        session.initialize()
        properties = session.request("tools/list", {})["tools"][0]["inputSchema"][
            "properties"
        ]
        self.assertIn("sql", properties)
        self.assertNotIn("r", properties)
        session.expect("1234567", sql="SELECT 1234567 AS answer")
        inspection = session.send(requirements={"action": "get"})
        self.assertEqual(
            inspection["structuredContent"]["requirements"]["duckdb"],
            ["icu", "json", "sqlite"],
        )
        session.expect("Count", sql="CREATE TABLE retained AS SELECT 1234567 AS answer")
        session.expect("1234567", sql="SELECT * FROM retained")
        exercise_sql_interrupt(session)
        session.expect(
            "selected sqlite",
            # fmt: python
            python=code("""
                import sqlite3

                selected = sqlite3.connect(":memory:")
                selected.execute("CREATE TABLE selected(answer INTEGER)")
                selected.execute("INSERT INTO selected VALUES (7654321)")
                _console.sql_connection(selected)
                print("selected sqlite")
                """),
        )
        session.expect("7654321", sql="SELECT * FROM selected")
        session.expect("[done]", python="_console.sql_connection(None)")
        session.expect("1234567", sql="SELECT * FROM retained")
        session.expect("worker stopped", control="restart")
        result = session.send(
            sql="SELECT count(*) FROM duckdb_tables() WHERE table_name = 'retained'"
        )
        self.assertFalse(result.get("isError"), result)
        self.assertTrue(result["content"][0]["text"].endswith("0\n"), result)
        session.expect("1234567", sql="SELECT 1234567 AS answer")
        result = session.send(
            sql="SELECT sum(i::DOUBLE) FROM range(1000000000000) AS values(i)",
            timeout_ms=1000,
        )
        self.assertIn("running;", json.dumps(result))
        session.expect("1234567", control="restart", sql="SELECT 1234567 AS answer")

    def test_sql_with_r(self):
        session = self.session(
            environment=dict(
                r_duckdb_snapshot_environment(), RETICULATE_PYTHON=sys.executable
            )
        )
        exercise_r_sql(session)

    def test_startup_without_processor_architecture(self):
        # Codex clears the inherited environment and keeps WINDOWS_CORE_ENV_VARS:
        # https://github.com/openai/codex/blob/ca466061d64f0b44f416135c7fd06aa7af850bbc/codex-rs/protocol/src/shell_environment.rs#L170-L201
        # Use its core allowlist without optional certificate/config overrides.
        # R DuckDB teardown can crash without PROCESSOR_ARCHITECTURE, before a cell.
        allowed = {
            "PATH",
            "PATHEXT",
            "SHELL",
            "COMSPEC",
            "SYSTEMROOT",
            "WINDIR",
            "SYSTEMDRIVE",
            "USERNAME",
            "USERDOMAIN",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "PROGRAMFILES",
            "PROGRAMFILES(X86)",
            "PROGRAMW6432",
            "PROGRAMDATA",
            "LOCALAPPDATA",
            "APPDATA",
            "TEMP",
            "TMP",
            "TMPDIR",
            "POWERSHELL",
            "PWSH",
        }
        environment = {
            name: value for name, value in os.environ.items() if name.upper() in allowed
        }
        # Make the configured test R discoverable through the retained PATH;
        # R_HOME and explicit Python selections must not mask client filtering.
        _, rscript = r_test_environment()
        environment["PATH"] = os.pathsep.join(
            (str(rscript.parent), environment.get("PATH", ""))
        )
        for name in ("PROCESSOR_ARCHITECTURE", "R_HOME", "RETICULATE_PYTHON"):
            self.assertNotIn(name, environment)
        for filtered, value in ((False, None), (True, None), (False, "")):
            with self.subTest(filtered=filtered, value=value):
                launch_environment = environment.copy()
                if value is not None:
                    launch_environment["PROCESSOR_ARCHITECTURE"] = value
                overrides = ()
                if filtered:
                    overrides = (
                        "inherit_environment=false",
                        "environment=" + json.dumps(launch_environment),
                    )
                session = Session(launch_environment, overrides=overrides)
                try:
                    session.initialize()
                    session.request("tools/list", {})
                    session.expect("R_SMOKE 4", r='cat("R_SMOKE", 2 + 2, "\\n")')
                    inspection = session.send(requirements={"action": "get"})
                    self.assertFalse(inspection.get("isError"), inspection)
                    self.assertTrue(inspection["structuredContent"]["prepared"])
                    session.expect(
                        "PYTHON_SMOKE 4",
                        # fmt: python
                        python=code("""
                            import os

                            assert os.environ["PROCESSOR_ARCHITECTURE"] == "AMD64"
                            print("PYTHON_SMOKE", 2 + 2)
                            """),
                    )
                    session.expect("1234567", sql="SELECT 1234567 AS sql_smoke")
                    session.expect(
                        "RESTART_SMOKE 4",
                        control="restart",
                        python="print('RESTART_SMOKE', 2 + 2)",
                    )
                finally:
                    session.close()

    def test_sql_startup_without_r(self):
        environment = dict(
            os.environ,
            PATH=str(Path(os.environ["SystemRoot"]) / "System32"),
            RETICULATE_PYTHON=sys.executable,
        )
        environment.pop("R_HOME", None)
        # fmt: python
        source = code("""
            import sqlite3

            startup_count = globals().get("startup_count", 0) + 1
            native = sqlite3.connect(":memory:")
            _ = native.execute("CREATE TABLE selected AS SELECT 1234567 AS answer")
            _console.sql_connection(native)
            """)
        session = Session(
            environment,
            overrides=(
                'languages=["sql"]',
                "startup=" + json.dumps({"language": "python", "code": source}),
            ),
        )
        self.addCleanup(session.close)
        session.initialize()
        properties = session.request("tools/list", {})["tools"][0]["inputSchema"][
            "properties"
        ]
        self.assertEqual(set(properties) & {"r", "python", "sql"}, {"sql"})
        for restart in (False, True):
            session.expect(
                "1234567",
                sql="SELECT * FROM selected",
                **({"control": "restart"} if restart else {}),
            )
            session.expect("[done]", sql="UPDATE selected SET answer = 7654321")
            session.expect("7654321", sql="SELECT * FROM selected")

    def test_managed_python_requirements_without_r(self):
        uv = shutil.which("uv")
        self.assertIsNotNone(uv, "Windows resolver acceptance requires uv")
        directory = tempfile.TemporaryDirectory(prefix="console managed 日本語 ")
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        shutil.copyfile(uv, root / "uv.exe")
        environment = dict(
            os.environ,
            PATH=os.pathsep.join(
                [
                    str(root),
                    str(Path(sys.executable).parent),
                    str(Path(os.environ["SystemRoot"]) / "System32"),
                ]
            ),
            RETICULATE_PYTHON="managed",
            # Accept an existing uv-managed interpreter when no system install
            # is exposed by this fixture's PATH (for example, a uv-created venv).
            UV_PYTHON_PREFERENCE="system",
            UV_PYTHON_DOWNLOADS="never",
        )
        environment.pop("R_HOME", None)
        session = Session(environment)
        self.addCleanup(session.close)
        session.initialize()

        def completed(**arguments):
            result = session.send(**arguments)
            self.assertFalse(result.get("isError"), result)
            return result

        result = completed(
            requirements={"action": "set", "python": ["six"]},
            python="import six; saved = 42; print(six.__version__)",
        )
        self.assertIn("1.", json.dumps(result))
        result = completed(
            # fmt: python
            python=code("""
                try:
                    import sniffio
                except ModuleNotFoundError as error:
                    assert type(error) is ModuleNotFoundError
                    assert error.args == ("No module named 'sniffio'",), error.args
                    assert error.name == "sniffio", error.name
                    assert error.path is None, error.path
                    print("undeclared import preserves native ModuleNotFoundError")
                else:
                    raise AssertionError("undeclared dependency imported")
                """),
        )
        self.assertIn(
            "undeclared import preserves native ModuleNotFoundError", json.dumps(result)
        )
        inspection = session.send(requirements={"action": "get"})
        self.assertEqual(
            inspection["structuredContent"]["requirements"]["python"], ["six"]
        )
        result = completed(
            requirements={"python": ["sniffio"]},
            python="import sniffio; print(sniffio.__version__)",
        )
        self.assertIn("1.", json.dumps(result))
        inspection = session.send(requirements={"action": "get"})
        self.assertEqual(
            inspection["structuredContent"]["requirements"]["python"],
            ["six", "sniffio"],
        )
        failed = session.send(
            control="restart",
            requirements={"action": "set", "python": ["six==0"]},
        )
        self.assertTrue(failed.get("isError"), failed)
        inspection = session.send(requirements={"action": "get"})
        self.assertEqual(
            inspection["structuredContent"]["requirements"]["python"],
            ["six", "sniffio"],
        )
        self.assertIn("42", json.dumps(completed(python="print(saved)")))
        result = completed(
            control="restart", python="import six, sniffio; print('saved' in globals())"
        )
        self.assertIn("False", json.dumps(result))

    def test_managed_r_requirements_preserve_live_state(self):
        session = self.session()
        self.assertIn("42", json.dumps(session.send(r="saved <- 42L; saved")))
        result = session.send(
            requirements={"r": ["digest"]},
            r="stopifnot(saved == 42L); digest::digest('prepared')",
        )
        self.assertFalse(result.get("isError"), result)
        inspection = session.send(requirements={"action": "get"})
        self.assertIn("digest", inspection["structuredContent"]["requirements"]["r"])
        result = session.send(
            control="restart",
            r="stopifnot(!exists('saved')); digest::digest('prepared')",
        )
        self.assertFalse(result.get("isError"), result)

    def test_python_initializes_before_r(self):
        session = Session(defer_bootstrap=True)
        self.addCleanup(session.close)
        session.initialize()
        # fmt: python
        result = session.send(
            python=dedent("""
                import ctypes
                import sys

                assert ctypes.windll.kernel32.GetModuleHandleW("R.dll") == 0
                python_value = 41
                print("python without R")
                """)
        )
        self.assertFalse(result.get("isError"), result)
        self.assertIn("python without R", json.dumps(result))
        self.assertNotIn("AssertionError", json.dumps(result))
        self.assertIn("42", json.dumps(session.send(r="r_value <- 42; r_value")))
        self.assertIn("42", json.dumps(session.send(python="print(python_value + 1)")))
        self.assertIn("42", json.dumps(session.send(r="r_value")))
        self.assertIn(
            "41", json.dumps(session.send(r="reticulate::py_eval('python_value')"))
        )
        self.assertIn("41", json.dumps(session.send(r="py$python_value")))
        self.assertIn("42", json.dumps(session.send(python="r.r_value")))

    def relay_session(self, scenario):
        directory = tempfile.TemporaryDirectory(prefix="console relay ")
        self.addCleanup(directory.cleanup)
        executable = Path(directory.name) / "relay.exe"
        subprocess.run(
            [
                "rustc",
                "--edition=2024",
                str(ROOT / "tests/fixtures/windows_relay.rs"),
                "-o",
                str(executable),
            ],
            check=True,
        )
        session = Session(
            dict(
                os.environ,
                TEST_RELAY_SCENARIO=scenario,
                TEST_CONSOLE_BINARY=str(NATIVE_BINARY),
            ),
            relay=executable,
        )
        self.addCleanup(session.close)
        session.timeout = 15
        session.initialize()
        return session

    def test_restart_after_forced_relay_retirement(self):
        session = self.relay_session("stall_shutdown")
        self.assertIn("42", json.dumps(session.send(r="42")))
        result = session.send(control="restart", r="42", timeout_ms=10000)
        self.assertFalse(result.get("isError"), result)
        self.assertIn("42", json.dumps(result))
        self.assertIn("42", json.dumps(session.send(r="42")))

    def test_restart_while_relay_exits_on_shutdown(self):
        session = self.relay_session("exit_shutdown")
        self.assertIn("42", json.dumps(session.send(r="42")))
        for generation in range(12):
            with self.subTest(generation=generation):
                result = session.send(control="restart", r="42", timeout_ms=10000)
                self.assertFalse(result.get("isError"), result)
                self.assertIn("42", json.dumps(result))
        (recording,) = (
            Path(session.directory.name) / ".agents/console/sessions"
        ).iterdir()
        source_documents = [
            recording / f"sessions/session-{session_id:03}.qmd"
            for session_id in range(1, 14)
        ]
        self.assertEqual(
            sorted((recording / "sessions").glob("*.qmd")), source_documents
        )
        self.assertFalse((recording / "transcript.qmd").exists())
        for path in (
            recording / "capture/events.jsonl",
            recording / "transcript.md",
            *source_documents,
        ):
            self.assertTrue(path.stat().st_mode & 0o222, path)
        for path in source_documents:
            self.assertIn("```{r}\n#| error: true\n42\n```", path.read_text())
        session.process.stdin.close()
        self.assertEqual(session.process.wait(timeout=15), 0)
        events = [
            json.loads(line)
            for line in (recording / "capture/events.jsonl")
            .read_text(encoding="utf-8")
            .splitlines()
        ]
        workers = [event for event in events if event["event"] == "worker_started"]
        cells = [event["owner"] for event in events if event["event"] == "cell_started"]
        self.assertEqual(len(workers), 13)
        self.assertEqual(
            [(owner["session_id"], owner["cell_id"]) for owner in cells],
            [(session_id, 1) for session_id in range(1, 14)],
        )
        for session_id in range(1, 14):
            self.assertEqual(
                (
                    recording / f"capture/sessions/{session_id:03}/cell-001.txt"
                ).read_text(),
                "42\n",
            )

        for path in recording.rglob("*"):
            if path.is_file():
                self.assertFalse(path.stat().st_mode & 0o222, path)

        for transition in ("worker_started", "worker_ready"):
            self.assertEqual(
                [
                    event["session_id"]
                    for event in events
                    if event["event"] == transition
                ],
                list(range(1, 14)),
            )

    def test_relay_setup_error_reaches_mcp(self):
        session = self.relay_session("block_sideband")
        result = session.send(r="42")
        self.assertTrue(result.get("isError"), result)
        text = "\n".join(item.get("text", "") for item in result["content"])
        self.assertIn(
            text,
            {
                f"[failed to create worker sideband: {ctypes.FormatError(code).strip()} (os error {code})]"
                for code in (5, 231)  # Access denied or all pipe instances busy.
            },
        )

    def test_startup_eof_cancels_python_inspection(self):
        with tempfile.TemporaryDirectory(prefix="console startup ") as directory:
            root = Path(directory)
            subprocess.run(
                [
                    "rustc",
                    "--edition=2024",
                    str(ROOT / "tests/fixtures/windows_inspection.rs"),
                    "-o",
                    str(root / "python.exe"),
                ],
                check=True,
            )
            ready = Gate(timeout=8)
            self.addCleanup(ready.close)
            environment = dict(
                os.environ,
                PATH=str(root) + os.pathsep + os.environ["PATH"],
                RETICULATE_PYTHON=str(root / "python.exe"),
                TEST_INSPECTION_GATE=ready.name,
            )
            process = subprocess.Popen(
                [str(BINARY), "serve", "--no-sandbox"],
                cwd=root,
                env=environment,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel.OpenProcess.argtypes = [
                ctypes.c_uint32,
                ctypes.c_int,
                ctypes.c_uint32,
            ]
            kernel.OpenProcess.restype = ctypes.c_void_p
            kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
            kernel.WaitForSingleObject.restype = ctypes.c_uint32
            kernel.TerminateProcess.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
            kernel.CloseHandle.argtypes = [ctypes.c_void_p]
            inspection = None
            try:
                ready.accept(process)
                pid = int(ready.readline())
                inspection = kernel.OpenProcess(0x100001, False, pid)
                self.assertTrue(inspection, ctypes.get_last_error())
                self.assertEqual(kernel.WaitForSingleObject(inspection, 0), 258)
                # communicate closes MCP input while inspection awaits release.
                _, errors = process.communicate(timeout=8)
                self.assertNotEqual(
                    process.returncode, 0, errors.decode(errors="replace")
                )
                self.assertEqual(
                    kernel.WaitForSingleObject(inspection, 0),
                    0,
                    "server EOF returned before inspection retirement",
                )
            finally:
                with ExitStack() as cleanup:
                    cleanup.callback(ready.close)
                    if inspection:
                        cleanup.callback(kernel.CloseHandle, inspection)
                    cleanup.callback(process.communicate, timeout=5)
                    if process.poll() is None:
                        process.kill()
                    if inspection and kernel.WaitForSingleObject(inspection, 0) != 0:
                        kernel.TerminateProcess(inspection, 1)
                        self.assertEqual(
                            kernel.WaitForSingleObject(inspection, 5000), 0
                        )

    def test_python_inspection_descendants_are_retired(self):
        with tempfile.TemporaryDirectory(prefix="console resolver ") as directory:
            root = Path(directory)
            source = root / "resolver.rs"
            source.write_text(r"""fn main() {
    if std::env::args().any(|arg| arg == "child") {
        std::thread::sleep(std::time::Duration::from_secs(60));
    } else {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("child").spawn().unwrap();
        std::fs::write(std::env::var("TEST_RESOLVER_PID").unwrap(), child.id().to_string()).unwrap();
        println!("ir 0.0.0");
    }
}
""")
            subprocess.run(
                ["rustc", str(source), "-o", str(root / "python.exe")], check=True
            )
            environment = dict(
                os.environ,
                PATH=str(root) + os.pathsep + os.environ["PATH"],
                RETICULATE_PYTHON=str(root / "python.exe"),
                TEST_RESOLVER_PID=str(root / "child.pid"),
            )
            session = Session(environment)
            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel.OpenProcess.restype = ctypes.c_void_p
            kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
            kernel.CloseHandle.argtypes = [ctypes.c_void_p]
            try:
                session.initialize()
                result = session.send(r="1L")
                self.assertTrue(result.get("isError"), result)
                self.assertIn(
                    "invalid selected Python configuration", json.dumps(result)
                )
                pid = int((root / "child.pid").read_text())
                handle = kernel.OpenProcess(0x100000, False, pid)
                if handle:
                    try:
                        self.assertEqual(kernel.WaitForSingleObject(handle, 1000), 0)
                    finally:
                        kernel.CloseHandle(handle)
            finally:
                pid_file = root / "child.pid"
                if pid_file.exists():
                    # Only the fixture-owned child recorded by this invocation.
                    subprocess.run(
                        ["taskkill", "/PID", pid_file.read_text(), "/F"],
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                session.close()

    def session(self, *, environment=None, defer_bootstrap=False):
        session = Session(environment, defer_bootstrap=defer_bootstrap)
        self.addCleanup(session.close)
        session.initialize()
        return session

    def test_r_persistence_and_restart(self):
        session = self.session()
        schema = session.request("tools/list", {})
        self.assertEqual([tool["name"] for tool in schema["tools"]], ["send"])
        self.assertNotIn(
            "SIGINT",
            schema["tools"][0]["inputSchema"]["properties"]["control"]["description"],
        )
        result = session.send(r="windows_value <- 40L; windows_value + 2L")
        self.assertIn("42", json.dumps(result))
        self.assertEqual(
            session.send()["content"], [{"type": "text", "text": "\n[idle]"}]
        )
        result = session.send(r="windows_value + 3L")
        self.assertIn("43", json.dumps(result))
        session.send(control="restart")
        result = session.send(r="exists('windows_value')")
        self.assertIn("FALSE", json.dumps(result))

    def test_r_startup_paths_survive_later_cells_and_restart(self):
        with tempfile.TemporaryDirectory(prefix="console user home ") as user_home:
            session = Session(
                dict(os.environ, R_USER=user_home, RETICULATE_PYTHON=sys.executable)
            )
            self.addCleanup(session.close)
            session.initialize()
            for generation in range(2):
                if generation:
                    session.send(control="restart")
                session.send(r="invisible(gc())")
                result = session.send(
                    # fmt: r
                    r=dedent("""
                        stopifnot(
                          identical(normalizePath(R.home()), normalizePath(Sys.getenv("R_HOME"))),
                          identical(
                            normalizePath(path.expand("~")),
                            normalizePath(Sys.getenv("R_USER"))
                          )
                        )
                        loadNamespace("splines")
                        stopifnot(file.exists(system.file("DESCRIPTION", package = "splines")))
                        cat("startup paths intact")
                        """).strip()
                )
                self.assertFalse(result.get("isError"), result)
                self.assertIn("startup paths intact", json.dumps(result))

    def test_native_r_startup_and_captured_vanilla_override(self):
        for vanilla in (False, True):
            with (
                self.subTest(vanilla=vanilla),
                tempfile.TemporaryDirectory() as directory,
            ):
                home = Path(directory)
                environ = home / ".Renviron"
                profile = home / ".Rprofile"
                environ.write_text(
                    "CONSOLE_NATIVE_ENV=first\n"
                    "CONSOLE_NATIVE_ENV_READS=${CONSOLE_NATIVE_ENV_READS}x\n"
                    "R_DEFAULT_PACKAGES=utils\n"
                )
                profile.write_text(
                    # fmt: r
                    code("""
                        native_value <- Sys.getenv("CONSOLE_NATIVE_ENV")
                        options(width = 73L)
                        .First <- function() native_first <<- TRUE
                        """)
                )
                environment = dict(
                    os.environ,
                    HOME=str(home),
                    R_USER=str(home),
                    R_ENVIRON=os.devnull,
                    R_PROFILE=os.devnull,
                    R_ENVIRON_USER=str(environ),
                    R_PROFILE_USER=str(profile),
                    MCP_CONSOLE_LANGUAGES="r",
                )
                environment.pop("R_DEFAULT_PACKAGES", None)
                environment.pop("CONSOLE_NATIVE_ENV_READS", None)
                session = Session(
                    environment,
                    bare_r=True,
                    overrides=(f"r.vanilla={str(vanilla).lower()}",),
                    use_r_startup_files=True,
                )
                try:
                    session.initialize()
                    for value in ("first", "edited"):
                        if value == "edited":
                            config = (
                                Path(session.directory.name)
                                / ".agents/console/config.yaml"
                            )
                            config.write_text(
                                f"r:\n  vanilla: {str(not vanilla).lower()}\n"
                            )
                            environ.write_text(
                                "CONSOLE_NATIVE_ENV=edited\n"
                                "CONSOLE_NATIVE_ENV_READS=${CONSOLE_NATIVE_ENV_READS}x\n"
                                "R_DEFAULT_PACKAGES=utils\n"
                            )
                            session.send(control="restart")
                        if vanilla:
                            # fmt: r
                            check = code("""
                                stopifnot(
                                  "--vanilla" %in% commandArgs(),
                                  Sys.getenv("CONSOLE_NATIVE_ENV") == "",
                                  Sys.getenv("CONSOLE_NATIVE_ENV_READS") == "",
                                  !exists("native_value"),
                                  !exists("native_first"),
                                  "package:stats" %in% search()
                                )
                                cat("vanilla startup complete")
                                """)
                            expected = "vanilla startup complete"
                        else:
                            # fmt: r
                            check = code(f"""
                                stopifnot(
                                  !("--vanilla" %in% commandArgs()),
                                  "--no-save" %in% commandArgs(),
                                  identical(native_value, "{value}"),
                                  identical(Sys.getenv("CONSOLE_NATIVE_ENV_READS"), "x"),
                                  native_first,
                                  identical(getOption("width"), 73L),
                                  "package:utils" %in% search(),
                                  !("package:stats" %in% search())
                                )
                                cat("native startup complete")
                                """)
                            expected = "native startup complete"
                        result = session.send(r=check)
                        self.assertFalse(result.get("isError"), result)
                        self.assertIn(expected, json.dumps(result))
                finally:
                    session.close()

    def test_python_errors_preserve_state(self):
        session = self.session()
        result = session.send(
            # fmt: python
            python=dedent("""
                windows_value = 40
                windows_value + 2
                """).strip()
        )
        self.assertFalse(result.get("isError"), result)
        self.assertIn("42", json.dumps(result))
        result = session.send(python="windows_value + 3")
        self.assertIn("43", json.dumps(result))
        result = session.send(python="raise ValueError('windows error')")
        self.assertIn("ValueError: windows error", json.dumps(result))
        result = session.send(python="windows_value")
        self.assertIn("40", json.dumps(result))

    def test_interrupt_waiting_for_input(self):
        session = self.session()
        session.send(r="1L")
        result = session.observe("waiting for stdin", r="readline('Name: ')")
        self.assertIn("waiting for stdin", json.dumps(result))
        result = session.send(control="interrupt", timeout_ms=1000)
        self.assertNotIn("waiting for stdin", json.dumps(result))
        result = session.send(python="answer = input('Python: '); answer")
        self.assertIn("waiting for stdin", json.dumps(result))
        result = session.expect("hello", stdin="hello\n")
        self.assertIn("hello", json.dumps(result))

    def test_unicode_plots_and_raw_output(self):
        session = self.session()
        result = session.send(r='cat("caf\u00e9 \u03bb \u6f22\u5b57\\n"); plot(1:3)')
        self.assertIn(
            "caf\u00e9 \u03bb \u6f22\u5b57", json.dumps(result, ensure_ascii=False)
        )
        self.assertTrue(
            any(item["type"] == "image" for item in result["content"]), result
        )
        result = session.send(
            python='import os; os.write(1, b"raw stdout\\n"); os.write(2, b"raw stderr\\n"); "caf\u00e9 \u03bb \u6f22\u5b57"'
        )
        text = json.dumps(result, ensure_ascii=False)
        for expected in ["raw stdout", "raw stderr", "caf\u00e9 \u03bb \u6f22\u5b57"]:
            self.assertIn(expected, text)

    def test_python_plots_and_interrupt(self):
        environment = dict(os.environ)
        environment["RETICULATE_PYTHON"] = sys.executable
        session = Session(environment)
        self.addCleanup(session.close)
        session.initialize()
        result = session.send(
            # fmt: python
            python=dedent("""
                import packaging

                managed_value = 42
                managed_value
                """).strip()
        )
        self.assertFalse(result.get("isError"), result)
        self.assertIn("42", json.dumps(result))
        result = session.send(
            # fmt: python
            python=dedent("""
                import matplotlib.pyplot as plt

                plt.plot([1, 2, 3])
                plt.show()
                plt.close()
                """).strip()
        )
        self.assertTrue(
            any(item["type"] == "image" for item in result["content"]), result
        )
        result = session.observe(
            "Python loop entered",
            python="print('Python loop entered')\nwhile True: pass",
        )
        self.assertIn("running", json.dumps(result))
        result = session.send(control="interrupt", timeout_ms=2000)
        self.assertNotIn("running;", json.dumps(result))
        result = session.send(python="managed_value")
        self.assertIn("42", json.dumps(result))

    def test_summarizes_empty_cell_after_oversized_startup_output(self):
        with tempfile.TemporaryDirectory(prefix="console startup ") as directory:
            root = Path(directory)
            (root / "sitecustomize.py").write_text(
                # fmt: python
                dedent("""
                    import sys

                    if "_mcp_console_services" in sys.modules:
                        print("startup head")
                        print("s" * 32768)
                        print("startup tail")
                    """)
            )
            environment = dict(
                os.environ,
                RETICULATE_PYTHON=sys.executable,
                RETICULATE_PYTHONPATH=str(root),
                MCP_CONSOLE_LANGUAGES="python",
            )
            session = Session(environment)
            try:
                session.initialize()
                raw = "startup head\n" + "s" * 32768 + "\nstartup tail\n"
                sessions = Path(session.directory.name) / ".agents/console/sessions"
                deadline = time.monotonic() + session.timeout
                while True:
                    session.send(requirements={"action": "get"})
                    logs = list(sessions.glob("*/capture/sessions/001/startup.txt"))
                    if logs and logs[0].read_text() == raw:
                        break
                    self.assertLess(
                        time.monotonic(), deadline, "startup log was not retained"
                    )
                result = session.send(python="pass")
                self.assertFalse(result["isError"], result)
                text = "".join(item.get("text", "") for item in result["content"])
                self.assertLessEqual(len(text.encode()), 8192)
                self.assertTrue(text.startswith("startup head\n"), text)
                self.assertTrue(text.endswith("startup tail\n"), text)
                (recording,) = (
                    Path(session.directory.name) / ".agents/console/sessions"
                ).iterdir()
                self.assertEqual(
                    (recording / "capture/sessions/001/startup.txt").read_text(),
                    raw,
                )
                (cell_log,) = (recording / "capture/sessions/001").glob("cell-*.txt")
                self.assertEqual(cell_log.read_bytes(), b"")
                self.assertIn("42", json.dumps(session.send(python="42")))
            finally:
                session.close()

    def test_worker_exit_and_output_retention(self):
        session = self.session()
        result = session.send(r='cat(strrep("x", 20000))')
        self.assertLessEqual(
            sum(len(item.get("text", "").encode()) for item in result["content"]), 8192
        )
        records = Path(session.directory.name) / ".agents/console/sessions"
        self.assertTrue(records.is_dir())
        self.assertTrue(
            any(
                b"x" * 20000 in path.read_bytes()
                for path in records.rglob("*")
                if path.is_file()
            )
        )
        result = session.send(r='quit(save="no", status=7L)')
        self.assertIn("exited", json.dumps(result))
        result = session.send(r="42L")
        self.assertIn("42", json.dumps(result))

    def test_eof_retires_running_worker(self):
        session = self.session()
        result = session.send(r="cat(Sys.getpid()); repeat {}", timeout_ms=3000)
        self.assertIn("running", json.dumps(result))
        # close() has a bounded wait and fails if it must kill the server.

    def test_worker_exit_with_continuous_descendant_output(self):
        session = self.session()
        root = Path(session.directory.name)
        writer = root / "writer.py"
        writer.write_text(
            # fmt: python
            dedent("""
                import os
                import time

                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    os.write(1, b"x" * 65536)
                """)
        )
        pid_file = root / "writer.pid"
        try:
            result = session.send(
                python=f"import subprocess, sys; from pathlib import Path; child = subprocess.Popen([sys.executable, {str(writer)!r}]); Path({str(pid_file)!r}).write_text(str(child.pid))"
            )
            self.assertFalse(result.get("isError"), result)
            session.timeout = 10
            result = session.send(r='quit(save="no", status=7L)', timeout_ms=5000)
            self.assertIn("exited", json.dumps(result))
            self.assertIn("42", json.dumps(session.send(r="42L")))
        finally:
            if pid_file.exists():
                subprocess.run(
                    ["taskkill", "/PID", pid_file.read_text(), "/F"],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )

    def test_stdin_preserves_control_bytes(self):
        session = self.session()
        result = session.expect(
            "26", r="as.integer(charToRaw(readline()))", stdin="\x1a\n"
        )
        self.assertIn("26", json.dumps(result))

    def test_input_and_interrupt(self):
        exercise_input_and_interrupt(self.session())

    def test_idle_later_callbacks(self):
        exercise_later_callbacks(self.session())

    def test_idle_later_callback_error_preserves_worker(self):
        session = self.session()
        self.assertFalse(session.send(requirements={"r": ["later"]})["isError"])
        # fmt: r
        r = code(r"""
            retained <- 41L
            run_callback <- function() {
              if (!file.exists("later-gate")) {
                later::later(run_callback, delay = 0.01)
                return(invisible(NULL))
              }
              later::later(
                function() {
                  retained <<- retained + 1L
                  cat("callback recovered\n")
                  stopifnot(file.create("later-recovered"))
                },
                delay = 0.01
              )
              stop("idle callback failure")
            }
            later::later(run_callback, delay = 0.01)
            """)
        self.assertFalse(session.send(r=r)["isError"])
        root = Path(session.directory.name)
        (root / "later-gate").touch()
        wait_for_path(root / "later-recovered", "callback after an R error")
        output = ""
        deadline = time.monotonic() + 10
        while "callback recovered" not in output:
            result = session.send(timeout_ms=10)
            self.assertFalse(result["isError"], result)
            output += "".join(
                item.get("text", "") for item in result.get("content", [])
            )
            self.assertLess(time.monotonic(), deadline, output)
            time.sleep(0.01)
        self.assertIn("idle callback failure", output)
        result = session.send(r="retained")
        self.assertEqual(result["content"], [{"type": "text", "text": "[1] 42\n"}])


if __name__ == "__main__":
    unittest.main()
