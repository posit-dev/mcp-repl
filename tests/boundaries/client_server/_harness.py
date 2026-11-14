# /// script
# requires-python = ">=3.11"
# dependencies = ["py-yaml12"]
# ///

from __future__ import annotations

import array
import json
import os
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Self

if os.name == "posix":
    import fcntl
    import termios


sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from support.assertions import last_result_text
from support.checkpoints import release_fixture_checkpoint, wait_for_checkpoint
from support.client import McpClient, TextReader
from support.execution import SANDBOXED, Execution
from support.processes import (
    capture_process_identity,
    child_process_identities,
    host_process_id,
    process_group_exists,
    stop_process_group,
    stop_process_id,
)

TEST_GATED_RESPONSE_SIZE = 128 * 1024

FIXTURE_CHECKPOINT_TIMEOUT_SECONDS = 15

TEST_EVENT_FIFO_NAME = "zod-test-events"

TEST_CONTROL_FIFO_NAME = "zod-test-control"

TEST_CLEANUP_FIFO_NAME = "zod-test-cleanup"

TEST_RESPONSE_QUERY_FIFO_NAME = "zod-test-response-query"

TEST_RESPONSE_RESULT_FIFO_NAME = "zod-test-response-result"

TEST_CONTROL_READY_NAME = "zod-test-control-ready"


class ZodFixtureControl:
    def __init__(self, root: Path | None = None) -> None:
        self.temporary_directory = (
            tempfile.TemporaryDirectory() if root is None else None
        )
        if root is None:
            assert self.temporary_directory is not None
            root = Path(self.temporary_directory.name)
        self.root = root
        self.event_reader: int | None = None
        self.control_writer: int | None = None
        self.cleanup_writer: int | None = None
        self.events: list[dict[str, object]] = []
        self.buffer = bytearray()
        self.cleanup_released = False

    def configure(self, environment: dict[str, str]) -> None:
        environment["TMPDIR"] = str(self.root)
        environment["ZOD_TEST_FIXTURE_CONTROL"] = "1"

    def connect(self, client: McpClient) -> None:
        if self.event_reader is not None:
            return
        directory = wait_for_marker(
            self.root,
            TEST_CONTROL_READY_NAME,
            client,
        ).parent
        event_reader = os.open(
            directory / TEST_EVENT_FIFO_NAME,
            os.O_RDONLY | os.O_NONBLOCK,
        )
        control_writer = os.open(
            directory / TEST_CONTROL_FIFO_NAME,
            os.O_WRONLY | os.O_NONBLOCK,
        )
        cleanup_writer = os.open(
            directory / TEST_CLEANUP_FIFO_NAME,
            os.O_WRONLY | os.O_NONBLOCK,
        )
        os.set_blocking(control_writer, True)
        os.set_blocking(cleanup_writer, True)
        self.event_reader = event_reader
        self.control_writer = control_writer
        self.cleanup_writer = cleanup_writer

    def send_control(self, operation: int, kind: str, **details: object) -> None:
        assert self.control_writer is not None
        payload = (
            json.dumps(
                {"operation": operation, "kind": kind, **details},
                separators=(",", ":"),
            ).encode()
            + b"\n"
        )
        assert os.write(self.control_writer, payload) == len(payload)

    def release_cleanup(self) -> None:
        if self.cleanup_released:
            return
        if self.cleanup_writer is not None:
            try:
                os.write(self.cleanup_writer, b"1")
            except BrokenPipeError:
                pass
            os.close(self.cleanup_writer)
            self.cleanup_writer = None
        self.cleanup_released = True

    def wait_for(self, operation: int, kind: str) -> dict[str, object]:
        return self.wait_for_any(operation, {kind})

    def wait_for_any(
        self,
        operation: int,
        kinds: set[str],
    ) -> dict[str, object]:
        deadline = time.monotonic() + FIXTURE_CHECKPOINT_TIMEOUT_SECONDS
        while True:
            event = next(
                (
                    event
                    for event in self.events
                    if event.get("operation") == operation
                    and event.get("kind") in kinds
                ),
                None,
            )
            if event is not None:
                return event
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(
                    f"Zod did not emit one of {sorted(kinds)!r} for request "
                    f"{operation}; " + self.diagnostics()
                )
            assert self.event_reader is not None
            readable, _, _ = select.select([self.event_reader], [], [], remaining)
            if not readable:
                continue
            chunk = os.read(self.event_reader, 4096)
            assert chunk, "Zod event channel closed; " + self.diagnostics()
            self.record_events(chunk)

    def record_events(self, chunk: bytes) -> None:
        self.buffer.extend(chunk)
        while b"\n" in self.buffer:
            line, _, remainder = self.buffer.partition(b"\n")
            self.buffer = bytearray(remainder)
            event = json.loads(line)
            assert isinstance(event, dict), event
            assert set(event) >= {"operation", "kind", "component"}, event
            self.events.append(event)

    def assert_before(
        self,
        first: tuple[int, str],
        second: tuple[int, str],
    ) -> None:
        positions = {
            (event["operation"], event["kind"]): index
            for index, event in enumerate(self.events)
        }
        assert positions[first] < positions[second], self.diagnostics()

    def record_client_event(self, operation: int, kind: str, **details: object) -> None:
        self.events.append(
            {
                "operation": operation,
                "kind": kind,
                "component": "client",
                **details,
            }
        )

    def diagnostics(self) -> str:
        started = {
            event["operation"]
            for event in self.events
            if event.get("kind") == "worker_operation_started"
        }
        completed = {
            event["operation"]
            for event in self.events
            if event.get("kind") == "worker_operation_completed"
        }
        trace = "\n".join(json.dumps(event, sort_keys=True) for event in self.events)
        return f"outstanding requests: {sorted(started - completed)}; event trace:\n{trace}"

    def wait_for_eof(self) -> None:
        assert self.event_reader is not None
        deadline = time.monotonic() + FIXTURE_CHECKPOINT_TIMEOUT_SECONDS
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(
                    "Zod event channel remained open after fixture cleanup; "
                    + self.diagnostics()
                )
            readable, _, _ = select.select([self.event_reader], [], [], remaining)
            if not readable:
                continue
            chunk = os.read(self.event_reader, 4096)
            if not chunk:
                assert not self.buffer, self.diagnostics()
                return
            self.record_events(chunk)

    def close(self) -> None:
        self.release_cleanup()
        if self.control_writer is not None:
            os.close(self.control_writer)
        if self.event_reader is not None:
            os.close(self.event_reader)
        if self.temporary_directory is not None:
            self.temporary_directory.cleanup()

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_error: object) -> None:
        self.close()


class ResponseGateObserver:
    def __init__(
        self,
        root: Path,
        stream: socket.socket,
        release: Path,
    ) -> None:
        self.root = root
        self.stream = stream
        self.release = release
        self.stop_requested = threading.Event()
        self.error: Exception | None = None
        self.responded = False
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _run(self) -> None:
        query_reader: int | None = None
        result_writer: int | None = None
        try:
            deadline = time.monotonic() + FIXTURE_CHECKPOINT_TIMEOUT_SECONDS
            while True:
                ready = list(self.root.glob(f"**/{TEST_CONTROL_READY_NAME}"))
                assert len(ready) <= 1, (
                    f"found multiple Zod fixture control channels: {ready!r}"
                )
                if ready:
                    directory = ready[0].parent
                    break
                if self.stop_requested.wait(0.01):
                    return
                assert time.monotonic() < deadline, (
                    "Zod did not create its response gate controls"
                )

            query_reader = os.open(
                directory / TEST_RESPONSE_QUERY_FIFO_NAME,
                os.O_RDONLY | os.O_NONBLOCK,
            )
            result_writer = os.open(
                directory / TEST_RESPONSE_RESULT_FIFO_NAME,
                os.O_WRONLY | os.O_NONBLOCK,
            )
            os.set_blocking(result_writer, True)
            while True:
                try:
                    query = os.read(query_reader, 1)
                    break
                except BlockingIOError:
                    if self.stop_requested.wait(0.01):
                        return
            assert query == b"1", query
            completed = self.release.is_file()
            if not completed:
                try:
                    queued = self.stream.recv(
                        64 * 1024,
                        socket.MSG_PEEK | socket.MSG_DONTWAIT,
                    )
                    completed = b"\n" in queued
                except BlockingIOError:
                    pass
            assert os.write(result_writer, b"1" if completed else b"0") == 1
            self.responded = True
        except Exception as error:
            self.error = error
        finally:
            if query_reader is not None:
                os.close(query_reader)
            if result_writer is not None:
                os.close(result_writer)

    def finish(self) -> None:
        self.thread.join(FIXTURE_CHECKPOINT_TIMEOUT_SECONDS)
        assert not self.thread.is_alive(), "Zod did not query the response gate"
        if self.error is not None:
            raise self.error
        assert self.responded

    def close(self) -> None:
        self.stop_requested.set()
        self.thread.join(1)
        assert not self.thread.is_alive(), "response gate observer did not stop"


def queued_socket_bytes(stream: socket.socket) -> int:
    available = array.array("i", [0])
    fcntl.ioctl(stream.fileno(), termios.FIONREAD, available, True)
    return available[0]


class SocketTextReader(TextReader):
    def wait_for_incomplete_response(
        self,
        request: int,
        minimum_complete_size: int,
        diagnostics: str,
    ) -> int:
        assert not self.buffer
        readable, _, _ = select.select(
            [self.stream],
            [],
            [],
            FIXTURE_CHECKPOINT_TIMEOUT_SECONDS,
        )
        assert readable, (
            f"response {request} did not reach the test gate; {diagnostics}"
        )
        pending = queued_socket_bytes(self.stream)
        assert 0 < pending < minimum_complete_size, (
            f"response {request} completed before the test gate; "
            f"buffered={pending}; minimum_complete_size={minimum_complete_size}; "
            f"{diagnostics}"
        )
        prefix = self.stream.recv(min(pending, 256), socket.MSG_PEEK)
        assert f'"id":{request},'.encode() in prefix, prefix
        assert b"\n" not in prefix, prefix
        return pending

    def release_completed_response(
        self,
        request: int,
        release: Path,
        diagnostics: str,
    ) -> None:
        deadline = time.monotonic() + FIXTURE_CHECKPOINT_TIMEOUT_SECONDS
        while True:
            remaining = deadline - time.monotonic()
            assert remaining > 0, (
                f"response {request} did not complete at the test gate; {diagnostics}"
            )
            readable, _, _ = select.select([self.stream], [], [], remaining)
            assert readable, (
                f"response {request} did not advance at the test gate; {diagnostics}"
            )
            pending = queued_socket_bytes(self.stream)
            assert pending > 0, (
                f"response stream closed at the test gate for {request}; {diagnostics}"
            )
            queued = self.stream.recv(pending, socket.MSG_PEEK)
            if b"\n" in queued:
                release.touch()
                return
            self.buffer.extend(self.stream.recv(pending))


class SocketGateMcpClient(McpClient):
    def __init__(
        self,
        binary: Path,
        arguments: tuple[str, ...],
        environment: dict[str, str],
        current_directory: Path,
    ) -> None:
        input_writer, input_reader = socket.socketpair()
        output_reader, output_writer = socket.socketpair()
        output_writer.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4 * 1024)
        output_reader.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024)
        self.output_buffer_sizes = (
            output_writer.getsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF),
            output_reader.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF),
        )
        assert max(self.output_buffer_sizes) < TEST_GATED_RESPONSE_SIZE
        process = subprocess.Popen(
            [binary, *arguments],
            env=environment,
            cwd=current_directory,
            stdin=input_reader,
            stdout=output_writer,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        output_writer.close()
        input_stream = input_writer.makefile("w", encoding="utf-8")
        input_writer.close()
        assert process.stderr is not None

        self.temporary_directory = None
        self.console_home = None
        self.writable_roots = []
        self._track_recording(environment, current_directory, True)
        self.process = process
        self.stdin = input_stream
        self.stdout = SocketTextReader(output_reader)
        self.stderr = TextReader(process.stderr)
        self.transcript = []
        self._next_request_id = 1
        self._issued_request_ids = set()
        self.input_reader = input_reader
        self.test_stdio_closed = False

    def wait_until_input_is_read(
        self,
        description: str,
        control: ZodFixtureControl,
    ) -> None:
        """Wait for the server's next read from the staged input socket.

        Tests write exactly one complete JSONL frame at a time. A later frame is
        withheld until this returns, so observing that later frame leave the
        socket proves the preceding frame passed through ServerTransport.receive.
        """
        deadline = time.monotonic() + FIXTURE_CHECKPOINT_TIMEOUT_SECONDS
        while queued_socket_bytes(self.input_reader) != 0:
            assert self.process.poll() is None, (
                f"mcp-console stopped before consuming {description}; "
                + control.diagnostics()
            )
            assert time.monotonic() < deadline, (
                f"mcp-console did not consume {description}; " + control.diagnostics()
            )
            time.sleep(0.001)

    def close_input_observer(self) -> None:
        if self.input_reader.fileno() != -1:
            self.input_reader.close()

    def close_test_stdio(self) -> None:
        if self.test_stdio_closed:
            return
        self.close_input_observer()
        self.stdout.close()
        self.test_stdio_closed = True


def expose_idle_input_request(client: McpClient, temporary_path: Path) -> None:
    requested = client.start_send(r="request input while idle")
    completed = wait_for_marker(
        temporary_path,
        "zod-idle-input-cell-completed",
        client,
    )
    client.receive(requested)
    assert last_result_text(client) == "[done]"

    release_fixture_checkpoint(
        completed.parent / "zod-release-idle-input-request", client=client
    )
    wait_for_marker(
        temporary_path,
        "zod-idle-input-request-processed",
        client,
    )
    client.send(session="default")
    assert last_result_text(client) == (
        '[input requested: "idle> "]\n[waiting for stdin]'
    )


def submit_prompted_stdin(
    client: McpClient,
    temporary_path: Path,
    stdin: str,
    marker: str,
    expected: str,
) -> None:
    poll_start = len(client.transcript)
    submitted = client.start_send(stdin=stdin)
    wait_for_marker(temporary_path, marker, client)
    client.receive(submitted)
    if last_result_text(client) != expected:
        assert last_result_text(client) == "\n[waiting for stdin]"
        client.send(session="default")
    assert last_result_text(client) == expected
    calls = client.transcript[poll_start:]
    submitted["result"] = calls[-1]["result"]
    client.transcript[poll_start:] = [submitted]


def expose_idle_sideband_output(
    client: McpClient,
    temporary_path: Path,
    marker: str | None = None,
) -> None:
    suffix = f"-{marker}" if marker else ""
    source = (
        f"start background sideband: {marker}"
        if marker
        else "start background sideband"
    )
    client.send(r=source)
    assert last_result_text(client) == "[done]", repr(last_result_text(client))
    started = wait_for_marker(
        temporary_path,
        f"zod-background-sideband-started{suffix}",
        client,
    )
    (started.parent / f"zod-release-background-sideband{suffix}").touch()
    wait_for_marker(
        temporary_path,
        f"zod-background-sideband-emitted{suffix}",
        client,
    )


def wait_for_marker(root: Path, name: str, client: McpClient) -> Path:
    return wait_for_checkpoint(
        lambda: find_marker(root, name),
        f"Zod checkpoint {name!r}",
        root=root,
        recursive=True,
        client=client,
        timeout=FIXTURE_CHECKPOINT_TIMEOUT_SECONDS,
    )


def find_marker(root: Path, name: str) -> Path | None:
    markers = [path for path in [root / name] if path.exists()]
    markers.extend(root.glob(f"sandbox-*/data/{name}"))
    assert len(markers) <= 1, f"found multiple {name} markers"
    return markers[0] if markers else None


def wait_for_stopped_worker(
    root: Path,
    recorded_workers: list[tuple[int, int]],
    client: McpClient,
    execution: Execution,
) -> tuple[Path, int, int]:
    deadline = time.monotonic() + FIXTURE_CHECKPOINT_TIMEOUT_SECONDS
    while True:
        for marker in root.rglob("zod-stop-continue-worker"):
            process_id, parent_id, process_group = map(
                int,
                marker.read_text(encoding="utf-8").split(),
            )
            if execution == SANDBOXED:
                process_id, parent_id, process_group = (
                    host_process_id(pid, client.process.pid)
                    for pid in (process_id, parent_id, process_group)
                )
            worker = (process_id, process_group)
            if worker not in recorded_workers:
                recorded_workers.append(worker)
            if execution == SANDBOXED:
                relay_status = read_process_status(parent_id)
                assert relay_status is not None, "stopped worker's relay exited"
                if sys.platform == "darwin":
                    # The native stage now execs the relay as its group leader.
                    # The server's direct child is the runner outside that group.
                    assert parent_id == process_group
                    (supervisor,) = child_process_identities(
                        capture_process_identity(client.process.pid)
                    )
                    expected_parent = supervisor[0]
                else:
                    # Linux keeps its namespace init above the relay.
                    expected_parent = process_group
                assert relay_status[:2] == (expected_parent, process_group), (
                    "stopped worker's relay changed its sandbox process boundary"
                )
                assert process_id != process_group, (
                    "stopped worker unexpectedly leads the relay process group"
                )
                assert process_group != os.getpgrp(), (
                    "stopped worker shares the test process group"
                )
            else:
                parent = read_process_status(parent_id)
                assert parent is not None and parent[0] == client.process.pid, (
                    "stopped worker is not the server relay's direct child"
                )
            status = read_process_status(process_id)
            if status is not None and status[2].startswith("T"):
                assert status[:2] == (parent_id, process_group), (
                    "stopped worker changed its process boundary"
                )
                # Consume this generation's checkpoint before its PIDs can
                # retire or be reused in the replacement's PID namespace.
                marker.unlink()
                return marker, process_id, process_group
        assert client.process.poll() is None, (
            "mcp-console stopped before its direct worker reached SIGSTOP"
        )
        assert time.monotonic() < deadline, (
            "direct worker did not enter the stopped process state within "
            f"{FIXTURE_CHECKPOINT_TIMEOUT_SECONDS} seconds"
        )
        time.sleep(0.01)


def wait_for_stopped_process(
    process_id: int,
    process_group: int,
    client: McpClient,
    description: str,
) -> None:
    deadline = time.monotonic() + FIXTURE_CHECKPOINT_TIMEOUT_SECONDS
    while True:
        status = read_process_status(process_id)
        assert status is not None, (
            f"{description} process exited before reaching SIGSTOP"
        )
        assert status[1] == process_group, f"{description} process changed groups"
        if status[2].startswith("T"):
            return
        assert client.process.poll() is None, (
            f"mcp-console stopped before {description} reached SIGSTOP"
        )
        assert time.monotonic() < deadline, (
            f"{description} did not reach SIGSTOP within "
            f"{FIXTURE_CHECKPOINT_TIMEOUT_SECONDS} seconds"
        )
        time.sleep(0.01)


def wait_for_path(path: Path, description: str, client: McpClient) -> None:
    observed = wait_for_marker(path.parent, path.name, client)
    assert observed == path, f"found a different path while waiting for {description}"


def read_process_status(process_id: int) -> tuple[int, int, str] | None:
    status = subprocess.run(
        [
            "ps",
            "-o",
            "ppid=",
            "-o",
            "pgid=",
            "-o",
            "state=",
            "-p",
            str(process_id),
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    if status.returncode == 1 and not status.stdout.strip():
        return None
    assert status.returncode == 0, status.stderr
    fields = status.stdout.split()
    assert len(fields) == 3, status.stdout
    return int(fields[0]), int(fields[1]), fields[2]


def continue_stopped_worker(process_id: int, process_group: int) -> None:
    status = read_process_status(process_id)
    assert status is not None, "stopped worker exited before SIGCONT"
    assert status[1] == process_group, "stopped worker changed process groups"
    assert status[2].startswith("T"), "worker was not stopped before SIGCONT"
    os.kill(process_id, signal.SIGCONT)


def wait_for_worker_retirement(
    process_id: int,
    process_group: int,
    client: McpClient,
    execution: Execution,
) -> None:
    deadline = time.monotonic() + FIXTURE_CHECKPOINT_TIMEOUT_SECONDS
    while read_process_status(process_id) is not None or (
        execution == SANDBOXED and process_group_exists(process_group)
    ):
        assert client.process.poll() is None, (
            "mcp-console stopped while retiring the old worker generation"
        )
        assert time.monotonic() < deadline, "restart did not retire the old worker"
        time.sleep(0.01)


def stop_recorded_worker(
    process_id: int, process_group: int, execution: Execution
) -> None:
    if execution == SANDBOXED:
        stop_process_group(process_group)
    status = read_process_status(process_id)
    if status is not None and status[1] == process_group:
        stop_process_id(process_id)


def read_worker_group(marker: Path) -> int:
    worker_group = int(marker.read_text(encoding="utf-8"))
    assert worker_group != os.getpgrp(), "Zod did not enter a dedicated process group"
    return worker_group


def wait_for_process_group_exit(process_group: int, client: McpClient) -> None:
    deadline = time.monotonic() + FIXTURE_CHECKPOINT_TIMEOUT_SECONDS
    while process_group_exists(process_group):
        assert client.process.poll() is None, "mcp-console stopped during restart"
        assert time.monotonic() < deadline, (
            "restart did not enforce its shutdown deadline"
        )
        time.sleep(0.01)
