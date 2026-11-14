#!/usr/bin/env -S uv run --script

import os
import json
import signal
import sys
import tempfile
import time
from contextlib import ExitStack, closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.server.test_startup import (
    gated_discovery,
)
from boundaries.client_server.lifecycle.test_startup import startup_fixture
from boundaries.client_server.python.test_startup import selected_python
from support.python import isolated_python
from support.progress import elapsed_progress, phase_progress, without_elapsed
from support.snapshots import execution_snapshots
from support.requirements import (
    LINUX_SANDBOX,
    NATIVE_FIXTURES,
    POSIX,
    PROCESS_EVENTS,
    R,
    command,
    requires,
)
from support.assertions import last_result_text
from support.allocations import AllocationProfile
from support.checkpoints import FifoCheckpoint, wait_for_checkpoint, wait_for_path
from support.client import McpClient
from support.discovery import discovery_environment
from support.events import Events
from support.execution import DIRECT, SANDBOXED, Execution, executions
from support.processes import (
    capture_process_identity,
    child_process_identities,
    host_process_id,
    kill_processes,
    live_processes,
)
from support.normalization import code
from support.native import LOADER_VARIABLE, build_interposer
from support.r import r_test_environment
from support.records import Transcript
from support.resolvers import (
    ir_run_records,
    local_resolver_owner,
    recording_ir_environment,
)
from support.suites import run_this_suite

RUNNING = "\n[running; poll with an empty send]"


@requires(NATIVE_FIXTURES)
def test_connection_closure_joins_preparation_owner(binary: Path) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary).resolve()
        python, _ = isolated_python(root)
        environment = selected_python(root, python)
        environment.pop("R_HOME", None)
        environment.update(
            {
                "PATH": str(root),
                LOADER_VARIABLE: str(
                    build_interposer(root, "preparation_reap_interposer")
                ),
                "MCP_CONSOLE_TEST_REAP_PID": str(root / "resolver-pid"),
                "MCP_CONSOLE_TEST_REAP_DONE": str(root / "reaped"),
            }
        )
        with McpClient(binary, DIRECT.serve(), environment, root) as client:
            client.initialize_and_list_tools()
            client.send(python="42")
            assert last_result_text(client) == "42\n"
            client.finish()
            assert (root / "resolver-pid").exists(), (
                "resolver did not acknowledge closure"
            )
            assert (root / "reaped").exists(), (
                "server exited before reaping preparation"
            )
            return [{"preparation_owner_joined_before_server_exit": True}]


@requires(NATIVE_FIXTURES, PROCESS_EVENTS)
def test_connection_closure_reaps_stalled_preparation_within_shutdown_budget(
    binary: Path,
) -> Transcript:
    return closes_stalled_preparation(binary, deny_kill=False)


@requires(NATIVE_FIXTURES, PROCESS_EVENTS)
def test_connection_closure_reports_failed_preparation_termination(
    binary: Path,
) -> Transcript:
    return closes_stalled_preparation(binary, deny_kill=True)


def closes_stalled_preparation(binary: Path, *, deny_kill: bool) -> Transcript:
    with (
        tempfile.TemporaryDirectory() as temporary,
        ExitStack() as resources,
    ):
        root = Path(temporary).resolve()
        blocked = resources.enter_context(
            closing(FifoCheckpoint.create(root / "blocked-close"))
        )
        python, _ = isolated_python(root)
        environment = selected_python(root, python)
        environment.pop("R_HOME", None)
        environment.update(
            {
                "PATH": str(root),
                LOADER_VARIABLE: str(
                    build_interposer(root, "preparation_reap_interposer")
                ),
                "MCP_CONSOLE_TEST_REAP_PID": str(root / "resolver-pid"),
                "MCP_CONSOLE_TEST_REAP_DONE": str(root / "reaped"),
                "MCP_CONSOLE_TEST_REAP_BLOCK_CLOSE": str(blocked.path),
            }
        )
        if deny_kill:
            environment["MCP_CONSOLE_TEST_REAP_DENY_KILL"] = str(root / "denied-kill")
        identity = None
        with McpClient(binary, DIRECT.serve(), environment, root) as client:
            try:
                client.initialize_and_list_tools()
                client.expect("42\n", python="42")
                client.stdin.close()
                blocked.wait("preparation received Close and remains alive")
                identity = capture_process_identity(
                    int((root / "resolver-pid").read_text())
                )
                _, errors = client.finish_with_standard_error(expected_exit_status=1)
                expected = "local resolver setup or retirement deadline exceeded"
                if deny_kill:
                    expected += "; cannot terminate local resolver: Operation not permitted (os error 1); retirement unconfirmed"
                    assert (root / "denied-kill").exists()
                    assert not (root / "reaped").exists()
                    assert live_processes([identity]), (
                        "fixture did not keep preparation alive"
                    )
                else:
                    assert (root / "reaped").exists(), "preparation was not reaped"
                    assert not live_processes([identity]), (
                        "preparation survived server exit"
                    )
                assert errors == expected + "\n", errors
                return [
                    {
                        "stalled_preparation_reaped_before_server_exit": not deny_kill,
                        "stderr": errors,
                    }
                ]
            finally:
                if identity is not None:
                    kill_processes([identity])


@requires(NATIVE_FIXTURES, PROCESS_EVENTS)
def test_reaps_preparation_after_unconfirmed_termination(binary: Path) -> Transcript:
    return retains_failed_preparation(binary, retry_on_close=False)


@requires(NATIVE_FIXTURES, PROCESS_EVENTS)
def test_retries_unconfirmed_preparation_termination_on_close(
    binary: Path,
) -> Transcript:
    return retains_failed_preparation(binary, retry_on_close=True)


def retains_failed_preparation(binary: Path, *, retry_on_close: bool) -> Transcript:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary).resolve()
        python, _ = isolated_python(root)
        environment = selected_python(root, python)
        environment.pop("R_HOME", None)
        environment.update(
            {
                "PATH": str(root),
                LOADER_VARIABLE: str(
                    build_interposer(root, "preparation_reap_interposer")
                ),
                "MCP_CONSOLE_TEST_REAP_PID": str(root / "resolver-pid"),
                "MCP_CONSOLE_TEST_REAP_DONE": str(root / "reaped"),
                "MCP_CONSOLE_TEST_REAP_CORRUPT": "1",
                "MCP_CONSOLE_TEST_REAP_DENY_KILL": str(root / "denied-kill"),
            }
        )
        if retry_on_close:
            environment["MCP_CONSOLE_TEST_REAP_DENY_ONCE"] = "1"
        identity = None
        with McpClient(binary, DIRECT.serve(), environment, root) as client:
            try:
                client.initialize_and_list_tools()
                client.expect("42\n", python="42")
                identity = local_resolver_owner(
                    capture_process_identity(client.process.pid), binary
                )
                (root / "resolver-pid").write_text(str(identity[0]))
                os.kill(identity[0], signal.SIGUSR1)
                wait_for_path(
                    root / "denied-kill",
                    "forced preparation termination was denied",
                    client=client,
                )
                rejected = client.send(control="restart")
                assert rejected["isError"], rejected
                assert "unconfirmed" in last_result_text(client), last_result_text(
                    client
                )
                assert live_processes([identity]), "preparation must still be alive"
                if not retry_on_close:
                    kill_processes([identity])
                    wait_for_path(
                        root / "reaped",
                        "server reaped the preparation child after its later exit",
                        client=client,
                    )
                _, errors = client.finish_with_standard_error(expected_exit_status=1)
                assert "cannot terminate local resolver" in errors, errors
                assert "retirement unconfirmed" in errors, errors
                assert (root / "reaped").exists(), "preparation was not reaped"
                assert not live_processes([identity])
                return [
                    {
                        "replacement_rejected": True,
                        "preparation_reaped": True,
                        "termination_retried_on_close": retry_on_close,
                        "original_retirement_error_retained": True,
                    }
                ]
            finally:
                if identity is not None:
                    kill_processes([identity])


@requires(POSIX)
@executions(DIRECT, SANDBOXED)
def test_invalid_early_cell_does_not_poison_default_startup(
    binary: Path, execution: Execution
) -> Transcript:
    with (
        tempfile.TemporaryDirectory() as temporary,
        ExitStack() as resources,
    ):
        root = Path(temporary).resolve()
        python, site = isolated_python(root)
        reached = resources.enter_context(
            closing(FifoCheckpoint.create(root / "probe"))
        )
        release = resources.enter_context(
            closing(FifoCheckpoint.create(root / "release"))
        )
        (site / "sitecustomize.py").write_text(
            # fmt: python
            code(f"""
                import sys
                from pathlib import Path

                if sys.argv[0] == "-c":
                    with Path({str(reached.path)!r}).open("wb", buffering=0) as reached:
                        assert reached.write(b"1") == 1
                    with Path({str(release.path)!r}).open("rb", buffering=0) as release:
                        assert release.read(1) == b"1"
                """),
        )
        environment = selected_python(root, python)
        environment.pop("R_HOME", None)
        environment["PATH"] = str(root)
        environment["UV_TOOL_DIR"] = str(root)
        with McpClient(
            binary,
            execution.serve(
                "-c",
                "cache=host",
                *(("--writable-root", str(root)) if execution == SANDBOXED else ()),
            ),
            environment,
            root,
        ) as client:
            try:
                reached.wait("selected Python inspection is blocked")
                client.initialize_and_list_tools()
                client.send(
                    r="stop('must not execute')",
                    requirements={"python": ["six"]},
                    timeout_ms=0,
                )
                assert without_elapsed(last_result_text(client)) == RUNNING
                assert (
                    phase_progress(last_result_text(client)) == "dependency preparation"
                )
                release.release()
                failure = client.send(session="default")
                assert failure["isError"]
                assert (
                    last_result_text(client)
                    == "R cells are unavailable in Python sessions without R"
                )
                client.send(python="answer = 42; answer")
                assert last_result_text(client) == "42\n", client.transcript[-1]
                return client.finish()[3:]
            finally:
                release.release()


@requires(command("rustc"), R)
def test_accepts_zero_timeout_cell_during_discovery(binary: Path) -> Transcript:
    environment, _ = r_test_environment()
    with gated_discovery(binary, r_home=Path(environment["R_HOME"])) as (
        client,
        release,
    ):
        client.initialize_and_list_tools()
        client.send(
            r='started <- get0("started", ifnotfound = 0L) + 1L; started', timeout_ms=0
        )
        assert without_elapsed(last_result_text(client)) == RUNNING
        client.send(session="default", timeout_ms=0)
        assert without_elapsed(last_result_text(client)) == RUNNING
        client.send(session="default", timeout_ms=250)
        assert without_elapsed(last_result_text(client)) == RUNNING
        age, silent = elapsed_progress(last_result_text(client))
        assert age >= 0.2 and silent
        client.send(r="stop('second cell must not execute')", timeout_ms=0)
        assert client.transcript[-1]["result"]["isError"]
        client.request("ping")
        release.release()
        client.response_timeout = 600
        client.send(session="default", timeout_ms=600_000)
        assert last_result_text(client) == "[1] 1\n"
        client.send(r="started")
        assert last_result_text(client) == "[1] 1\n"
        return client.finish()


def ready_without_send(
    binary: Path, execution: Execution, *, sans_r: bool, close: bool = False
) -> Transcript:
    with ExitStack() as resources:
        root = Path(resources.enter_context(tempfile.TemporaryDirectory())).resolve()
        reached = FifoCheckpoint.create(root / "ready")
        release = FifoCheckpoint.create(root / "release")
        resources.callback(reached.close)
        resources.callback(release.close)
        if sans_r:
            python, site = isolated_python(root)
            python_started = FifoCheckpoint.create(root / "python-ready")
            resources.callback(python_started.close)
            (site / "sitecustomize.py").write_text(
                # fmt: python
                code(f"""
                    import os
                    import sys
                    from pathlib import Path

                    if sys.argv[0] != "-c":
                        Path({str(root / "python-started")!r}).write_text(str(os.getpid()))
                        with Path({str(python_started.path)!r}).open("wb", buffering=0) as ready:
                            assert ready.write(b"1") == 1
                    """),
            )
            environment = selected_python(root, python)
            environment.pop("R_HOME", None)
            environment["PATH"] = str(root)
        else:
            environment, _ = r_test_environment()
            environment["MCP_CONSOLE_LANGUAGES"] = "r"
        environment.update(
            {
                "MCP_CONSOLE_TEST_RELAY_READ_DYLIB": str(
                    build_interposer(root, "relay_stdout_read_interposer")
                ),
                "MCP_CONSOLE_TEST_RELAY_READ_MATCH": '"kind":"ready"',
                "MCP_CONSOLE_TEST_RELAY_READ_BLOCKED": str(reached.path),
                "MCP_CONSOLE_TEST_RELAY_READ_RELEASE": str(release.path),
            }
        )
        launcher = code("""
            import os
            import sys

            os.environ["MCP_CONSOLE_TEST_RELAY_READ_PID"] = str(os.getpid())
            loader = "DYLD_INSERT_LIBRARIES" if sys.platform == "darwin" else "LD_PRELOAD"
            os.environ[loader] = os.environ.pop("MCP_CONSOLE_TEST_RELAY_READ_DYLIB")
            os.execv(sys.argv[1], sys.argv[1:])
            """)
        with McpClient(
            Path(sys.executable),
            (
                "-c",
                launcher,
                str(binary),
                *execution.serve(
                    *(("--writable-root", str(root)) if execution == SANDBOXED else ())
                ),
            ),
            environment,
            root,
            response_timeout=600,
        ) as client:
            try:
                # The relay reports Ready only after launching the real worker.
                reached.wait("default worker launched without send", timeout=600)
                owner = capture_process_identity(client.process.pid)
                descendants = []
                pending = [owner]
                while pending:
                    children = child_process_identities(pending.pop())
                    descendants.extend(children)
                    pending.extend(children)
                if close:
                    retirement = resources.enter_context(Events())
                    for pid, _, _ in descendants:
                        retirement.watch_process(pid)
                client.initialize_and_list_tools()
                tools = client.transcript[-1]["result"]
                if not sans_r:
                    client.transcript[-1]["result"] = "<configured R-only schema>"
                client.request("ping")
                if close:
                    try:
                        client.close()
                        # An exited broker can await host reaping after its
                        # owner exits. Require kernel exit evidence without
                        # granting any additional shutdown time.
                        survivors = sorted(
                            {pid for pid, _, _ in descendants} - retirement.wait(0)
                        )
                        assert not survivors, (
                            f"prelaunch resources survived MCP closure: {survivors}; "
                            f"captured identities: {descendants}; "
                            f"identities still present: {live_processes(descendants)}"
                        )
                        return [*client.finish(), {"prelaunch_resources_retired": True}]
                    finally:
                        kill_processes(descendants)
                release.release()
                client.send(requirements={"action": "get"})
                if sans_r:
                    python_started.wait("prelaunch initialized Python without a cell")
                cell = (
                    {"python": "import os; worker_pid = os.getpid(); worker_pid"}
                    if sans_r
                    else {"r": "worker_pid <- Sys.getpid(); worker_pid"}
                )
                client.send(**cell)
                identity = last_result_text(client)
                worker_pid = int(identity.removeprefix("[1] "))
                assert host_process_id(worker_pid, owner[0]) in {
                    identity[0] for identity in descendants
                }, "first send replaced the already launched worker"
                if sans_r:
                    assert identity == (root / "python-started").read_text() + "\n"
                client.transcript[-1]["result"]["content"][0]["text"] = (
                    "<prewarmed worker pid>\n"
                )
                client.send(
                    **({"python": "os.getpid()"} if sans_r else {"r": "Sys.getpid()"})
                )
                assert last_result_text(client) == identity
                client.transcript[-1]["result"]["content"][0]["text"] = (
                    "<same worker pid>\n"
                )
                assert client.request("tools/list")["result"] == tools
                client.transcript[-1]["result"] = "<unchanged from initialization>"
                return client.finish()
            finally:
                release.release()


@execution_snapshots
@executions(DIRECT, SANDBOXED)
@requires(NATIVE_FIXTURES)
def test_sans_r_worker_is_ready_without_send(
    binary: Path, execution: Execution
) -> Transcript:
    return ready_without_send(binary, execution, sans_r=True)


@execution_snapshots
@executions(DIRECT, SANDBOXED)
@requires(NATIVE_FIXTURES, PROCESS_EVENTS)
def test_connection_closure_retires_prelaunch_resources(
    binary: Path, execution: Execution
) -> Transcript:
    return ready_without_send(binary, execution, sans_r=True, close=True)


@requires(LINUX_SANDBOX, NATIVE_FIXTURES, PROCESS_EVENTS)
def test_connection_closure_retires_adopted_prelaunch_resources(
    binary: Path,
) -> Transcript:
    import ctypes

    libc = ctypes.CDLL(None, use_errno=True)
    # Adopt orphaned sandbox brokers so host reaping cannot erase their PIDs
    # before we verify that MCP closure has stopped their execution.
    pr_set_child_subreaper = 36
    assert libc.prctl(pr_set_child_subreaper, 1, 0, 0, 0) == 0, ctypes.get_errno()
    try:
        return ready_without_send(binary, SANDBOXED, sans_r=True, close=True)
    finally:
        while True:
            try:
                pid, _ = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                break
            if pid == 0:
                break
        assert libc.prctl(pr_set_child_subreaper, 0, 0, 0, 0) == 0, ctypes.get_errno()


@execution_snapshots
@executions(DIRECT, SANDBOXED)
@requires(R, NATIVE_FIXTURES, command("ir"), command("uv"))
def test_r_worker_is_ready_without_send(
    binary: Path, execution: Execution
) -> Transcript:
    return ready_without_send(binary, execution, sans_r=False)


@executions(DIRECT, SANDBOXED)
@requires(PROCESS_EVENTS, command("ir"), command("uv"))
def test_early_replacement_requirements_withholds_cell_until_prepared(
    binary: Path, execution: Execution
) -> Transcript:
    with startup_fixture(binary, execution, phase="preparation") as fixture:
        fixture.wait_for_resolver()
        client = fixture.client
        client.initialize_and_list_tools()
        client.request("ping")
        client.send(
            python='import importlib.util; assert importlib.util.find_spec("numpy") is None; counter = 1; counter',
            requirements={"action": "set"},
            timeout_ms=0,
        )
        assert without_elapsed(last_result_text(client)) == RUNNING
        assert phase_progress(last_result_text(client)) == "dependency preparation"
        client.send(python="counter += 1", timeout_ms=20)
        assert client.transcript[-1]["result"]["isError"]
        client.send(session="default", timeout_ms=20)
        assert without_elapsed(last_result_text(client)) == RUNNING
        assert phase_progress(last_result_text(client)) == "dependency preparation"
        fixture.release.release()
        client.response_timeout = 600
        client.send(session="default", timeout_ms=600_000)
        assert last_result_text(client) == "1\n", last_result_text(client)
        client.send(python="counter")
        assert last_result_text(client) == "1\n"
        fixture.wait_for_resolver_exit()
        return client.finish()


@execution_snapshots
@executions(DIRECT, SANDBOXED)
@requires(R, NATIVE_FIXTURES, PROCESS_EVENTS, command("ir"), command("uv"))
@requires(command("rustc"))
def test_early_requirements_select_candidate_before_default_preparation(
    binary: Path, execution: Execution
) -> Transcript:
    return early_requirements_with_pending_poll(binary, execution)


@execution_snapshots
@executions(DIRECT, SANDBOXED)
@requires(R, NATIVE_FIXTURES, PROCESS_EVENTS, command("ir"), command("uv"))
def test_startup_poll_deadline_preserves_unclaimed_output(
    binary: Path, execution: Execution
) -> Transcript:
    # Read ahead through bootstrap while retaining Ready and every later byte.
    # The finite poll deadline then covers delivery ownership after R is ready.
    with tempfile.TemporaryDirectory() as temporary, ExitStack() as resources:
        root = Path(temporary).resolve()
        ready = resources.enter_context(closing(FifoCheckpoint.create(root / "ready")))
        proceed = resources.enter_context(
            closing(FifoCheckpoint.create(root / "proceed"))
        )
        write_reached = resources.enter_context(
            closing(FifoCheckpoint.create(root / "write-reached"))
        )
        write_release = resources.enter_context(
            closing(FifoCheckpoint.create(root / "write-release"))
        )
        evaluated = root / "evaluated"
        environment, _ = r_test_environment()
        environment.update(
            {
                "MCP_CONSOLE_LANGUAGES": "r",
                "MCP_CONSOLE_TEST_RELAY_READ_DYLIB": os.pathsep.join(
                    str(build_interposer(root, fixture))
                    for fixture in (
                        "relay_stdout_read_interposer",
                        "response_write_interposer",
                    )
                ),
                "MCP_CONSOLE_TEST_RELAY_READ_MATCH": '"kind":"ready"',
                "MCP_CONSOLE_TEST_RELAY_READ_AWAIT_MATCH": '"kind":"runtime_initialized"',
                "MCP_CONSOLE_TEST_RELAY_READ_BLOCKED": str(ready.path),
                "MCP_CONSOLE_TEST_RELAY_READ_RELEASE": str(proceed.path),
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_REACHED": str(write_reached.path),
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_RELEASE": str(write_release.path),
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_MATCH": "[running; poll with an empty send]",
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_COMPLETE": "1",
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_ARMED": str(root / "write-armed"),
                "MCP_CONSOLE_TEST_EVALUATED": str(evaluated),
            }
        )
        launcher = code("""
            import os
            import sys

            os.environ["MCP_CONSOLE_TEST_RELAY_READ_PID"] = str(os.getpid())
            loader = "DYLD_INSERT_LIBRARIES" if sys.platform == "darwin" else "LD_PRELOAD"
            os.environ[loader] = os.environ.pop("MCP_CONSOLE_TEST_RELAY_READ_DYLIB")
            os.execv(sys.argv[1], sys.argv[1:])
            """)
        with McpClient(
            Path(sys.executable),
            (
                "-c",
                launcher,
                str(binary),
                *execution.serve(
                    "-c",
                    "cache=host",
                    "-c",
                    "r.packages=[]",
                    *(("--writable-root", str(root)) if execution == SANDBOXED else ()),
                ),
            ),
            environment,
            root,
            response_timeout=600,
        ) as client:
            try:
                ready.wait(
                    "bootstrapped worker's Ready frame is held",
                    timeout=600,
                    client=client,
                )
                client.initialize_and_list_tools()
                pending = client.start_send(session="default", timeout_ms=10_000)
                client.request("ping")
                (root / "write-armed").touch()
                submitted = client.start_send(
                    r='cat("1", file = Sys.getenv("MCP_CONSOLE_TEST_EVALUATED")); 42L',
                    timeout_ms=0,
                )
                write_reached.wait(
                    "early cell's running response write is held", client=client
                )
                # The held write returns only on release, so the recording
                # cannot yet hold this reply's delivery.
                client.recording_receipts = False
                client.receive(submitted)
                client.recording_receipts = True
                assert without_elapsed(last_result_text(client)) == RUNNING
                assert phase_progress(last_result_text(client)) == "startup"
                proceed.release()
                wait_for_path(evaluated, "accepted cell ran", client=client)
                journal = next(
                    root.glob(".agents/console/sessions/*/capture/events.jsonl")
                )

                def poll_result_recorded() -> Path | None:
                    lines = journal.read_text().rpartition("\n")[0].splitlines()
                    events = [json.loads(line) for line in lines]
                    poll_call = next(
                        event
                        for event in events
                        if event["event"] == "tool_call"
                        and event["request_id"] == pending["id"]
                    )
                    return (
                        journal
                        if any(
                            event["event"] == "tool_result"
                            and event["call_id"] == poll_call["call_id"]
                            for event in events
                        )
                        else None
                    )

                wait_for_checkpoint(
                    poll_result_recorded,
                    "startup poll deadline expires before response delivery",
                    root=journal,
                    client=client,
                    timeout=15,
                )
                write_release.release()
                client.receive(pending)
                assert pending["result"] == {
                    "content": [
                        {
                            "type": "text",
                            "text": "[previous send response delivery is still pending]",
                        }
                    ],
                    "isError": True,
                }, pending
                client.expect("[1] 42\n", timeout_ms=0)
                client.expect("\n[idle]")
                return client.finish()
            finally:
                write_release.release()
                proceed.release()


@execution_snapshots
@executions(DIRECT, SANDBOXED)
@requires(R, NATIVE_FIXTURES, PROCESS_EVENTS, command("ir"), command("uv"))
@requires(POSIX)
@requires(command("rustc"))
def test_startup_poll_becomes_idle_after_terminal_response_delivery(
    binary: Path, execution: Execution
) -> Transcript:
    return early_requirements_with_pending_poll(
        binary, execution, completed_response=True
    )


def early_requirements_with_pending_poll(
    binary: Path,
    execution: Execution,
    *,
    completed_response: bool = False,
) -> Transcript:
    r_environment, _ = r_test_environment()
    with (
        ExitStack() as resources,
        discovery_environment(r_home=Path(r_environment["R_HOME"])) as (
            probe_environment,
            discovery,
        ),
    ):
        root = discovery.root
        environment, record = recording_ir_environment(
            root, fail_requirement="tidyverse"
        )
        environment.pop("R_HOME", None)
        environment["PATH"] = os.pathsep.join(
            (probe_environment["PATH"], environment["PATH"])
        )
        prepared = FifoCheckpoint.create(root / "prepared")
        proceed = FifoCheckpoint.create(root / "proceed")
        resources.callback(prepared.close)
        resources.callback(proceed.close)
        write_reached = resources.enter_context(
            closing(FifoCheckpoint.create(root / "write-reached"))
        )
        write_release = resources.enter_context(
            closing(FifoCheckpoint.create(root / "write-release"))
        )
        evaluated = root / "evaluated"
        # Linux LD_PRELOAD splits on spaces, while the discovery executable
        # deliberately lives in a path containing spaces.
        interposers = Path(resources.enter_context(tempfile.TemporaryDirectory()))
        environment.update(
            {
                "MCP_CONSOLE_TEST_IR_BLOCK_REQUIREMENT": "DBI",
                "MCP_CONSOLE_TEST_IR_STARTED": str(prepared.path),
                "MCP_CONSOLE_TEST_IR_RELEASE": str(proceed.path),
                LOADER_VARIABLE: str(
                    build_interposer(interposers, "response_write_interposer")
                ),
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_REACHED": str(write_reached.path),
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_RELEASE": str(write_release.path),
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_MATCH": (
                    "[1] 42"
                    if completed_response
                    else "[running; poll with an empty send]"
                ),
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_COMPLETE": "1",
                "MCP_CONSOLE_TEST_RESPONSE_WRITE_ARMED": str(root / "write-armed"),
                "MCP_CONSOLE_TEST_EVALUATED": str(evaluated),
            }
        )
        with McpClient(
            binary,
            execution.serve(
                "-c",
                "cache=host",
                *(("--writable-root", str(root)) if execution == SANDBOXED else ()),
            ),
            environment,
            root,
            response_timeout=600,
        ) as client:
            discovery.wait("runtime discovery is blocked")
            if completed_response:
                client.transcript.append(
                    {
                        "scenario": "An admitted poll follows another call's terminal delivery and preserves its input.",
                        "fixture": "Admit a cell during blocked discovery, give one poll its terminal result, then submit another poll while the completed response's transport write is held. Observe its receipt in capture before releasing the write.",
                    }
                )
            client.initialize_and_list_tools()
            if not completed_response:
                pending = client.start_send(session="default", timeout_ms=600_000)
                client.request("ping")
            (root / "write-armed").touch()
            try:
                submitted = client.start_send(
                    r='cat("1", file = Sys.getenv("MCP_CONSOLE_TEST_EVALUATED")); 42L',
                    requirements={"action": "set"},
                    timeout_ms=0,
                )
                if completed_response:
                    # A written request may not yet own the early cell. Its
                    # running response proves admission before discovery can
                    # launch default preparation and consume the DBI gate.
                    client.receive(submitted)
                    assert without_elapsed(last_result_text(client)) == RUNNING, (
                        submitted
                    )
                    assert phase_progress(last_result_text(client)) == "startup"
                    submitted = client.start_send(session="default", timeout_ms=600_000)
                    discovery.release()
                    prepared.wait(
                        "initial polling does not block candidate preparation"
                    )
                    proceed.release()
                write_reached.wait(
                    "cell response is visible before its write settles",
                    client=client,
                    timeout=(
                        max(0, client.response_deadline() - time.monotonic())
                        if completed_response
                        else 10
                    ),
                )
                # The held write returns only on release, so the recording
                # cannot yet hold this reply's delivery.
                client.recording_receipts = False
                client.receive(submitted)
                client.recording_receipts = True
                if completed_response:
                    assert last_result_text(client) == "[1] 42\n"
                    pending = client.start_send(
                        stdin="retained input\n", timeout_ms=600_000
                    )
                    journal = next(
                        root.glob(".agents/console/sessions/*/capture/events.jsonl")
                    )

                    def pending_received() -> Path | None:
                        lines = journal.read_text().rpartition("\n")[0].splitlines()
                        return (
                            journal
                            if any(
                                event["event"] == "protocol"
                                and event["direction"] == "received"
                                and event["message"].get("id") == pending["id"]
                                for event in map(json.loads, lines)
                            )
                            else None
                        )

                    wait_for_checkpoint(
                        pending_received,
                        "poll received while terminal response delivery is held",
                        root=journal,
                        client=client,
                    )
                    assert "result" not in pending and "error" not in pending, pending
                else:
                    assert without_elapsed(last_result_text(client)) == RUNNING, (
                        submitted
                    )
                    assert phase_progress(last_result_text(client)) == "startup"
                    discovery.release()
                    prepared.wait(
                        "initial polling does not block candidate preparation"
                    )
                    proceed.release()
                # Releasing IR permits materialization; the cell marker proves
                # preparation, startup, and execution finished. Use the client's
                # preparation deadline, including its case-cleanup reserve.
                wait_for_path(
                    evaluated,
                    "accepted cell ran",
                    client=client,
                    timeout=max(0, client.response_deadline() - time.monotonic()),
                )
                write_release.release()
                client.receive(pending)
                if completed_response:
                    assert pending["result"] == {
                        "content": [{"type": "text", "text": "\n[idle]"}],
                        "isError": False,
                    }, pending
                    client.expect(
                        '[1] "retained input"\n',
                        # fmt: r
                        r=code(r"""
                            local({
                              connection <- suppressWarnings(file("/dev/stdin"))
                              on.exit(close(connection))
                              readLines(connection, n = 1)
                            })
                            """),
                    )
                    client.expect("[1] 43\n", r="43L")
                else:
                    assert pending["result"]["content"] == [
                        {"type": "text", "text": "[1] 42\n"}
                    ], pending
                    assert not pending["result"]["isError"]
                assert not any(
                    "tidyverse" in call["arguments"] for call in ir_run_records(record)
                )
                return client.finish()
            finally:
                write_release.release()
                proceed.release()


@executions(DIRECT, SANDBOXED)
def test_idle_stdin_preserves_used_worker_and_input(
    binary: Path, execution: Execution
) -> Transcript:
    with McpClient(binary, execution.serve()) as client:
        client.initialize_and_list_tools()
        initial = client.send(requirements={"action": "get"})["structuredContent"]
        client.transcript[-1]["result"] = "<committed default manifest>"
        client.send(stdin="retained input\n")
        changed = client.send(requirements={"action": "set"})
        assert changed["isError"] is True, changed
        assert "explicit restart" in str(changed), changed
        assert (
            client.send(requirements={"action": "get"})["structuredContent"] == initial
        )
        client.transcript[-1]["result"] = "<unchanged default manifest>"
        client.expect("[input requested: \"\"]\n'retained input'\n", python="input()")
        return client.finish()[3:]


@requires(NATIVE_FIXTURES)
@requires(command("rustc"))
def test_failed_discovery_discards_pending_recording(binary: Path) -> Transcript:
    with (
        tempfile.TemporaryDirectory() as directory,
        closing(AllocationProfile(Path(directory))) as profile,
        discovery_environment() as (environment, discovery),
    ):
        environment.update(profile.environment)
        with McpClient(binary, DIRECT.serve(), environment) as client:
            discovery.wait("runtime discovery is blocked")
            client.initialize_and_list_tools()
            discovery.release()
            failure = client.send(r="stop('must not execute')")
            assert failure["isError"] is True, failure
            profile.start()
            for _ in range(1024):
                assert client.send(r="stop('must not execute')") == failure
            client.request("ping")
            _, largest = profile.stop()
            # No growing vector of requests survives a retained startup failure.
            assert largest <= 256 * 1024, largest
            _, stderr = client.finish_with_standard_error(expected_exit_status=1)
            assert "fixture R discovery failed" in stderr, stderr
            return [{"failed_send_count": 1024, "result": failure}, {"stderr": stderr}]


if __name__ == "__main__":
    run_this_suite(__file__)
