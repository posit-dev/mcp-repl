import json
import os
import select
import socket
import subprocess
import tempfile
import time
from pathlib import Path
from queue import SimpleQueue
from threading import Thread
from typing import Any, Self, TextIO

from support.records import ToolResult, Transcript, TranscriptEntry

SERVER_SHUTDOWN_SECONDS = 11
SERVER_REAP_SECONDS = 2
CASE_RESPONSE_RESERVE_SECONDS = SERVER_SHUTDOWN_SECONDS + SERVER_REAP_SECONDS + 1


def server_identity(binary: Path) -> dict[str, str]:
    """The `Implementation` Console reports in handshakes and modern results."""
    version = subprocess.check_output([binary, "--version"], text=True).strip()
    return {
        "name": "mcp-console",
        "title": "MCP Console",
        "version": version.removeprefix("mcp-console "),
        "description": (
            "An interactive workbench for agents working in R, Python, and SQL."
        ),
        "websiteUrl": "https://github.com/posit-dev/mcp-console",
    }


class TextReader:
    """Descriptor-backed text with an explicit buffer and bounded reads."""

    def __init__(self, stream: TextIO | socket.socket) -> None:
        self.stream = stream
        self.buffer = bytearray()
        self.eof = False
        self.closed = False
        self.reader: Thread | None = None
        if os.name == "nt" and not isinstance(stream, socket.socket):
            # Winsock cannot select anonymous pipes. Blocking readers announce
            # complete chunks through sockets so transport deadlines stay event-driven.
            self.ready, self.wake = socket.socketpair()
            self.chunks: SimpleQueue[bytes | OSError] = SimpleQueue()

            def read_pipe() -> None:
                while not self.closed:
                    try:
                        chunk = os.read(stream.fileno(), 64 * 1024)
                    except OSError as error:
                        chunk = error
                    self.chunks.put(chunk)
                    try:
                        self.wake.sendall(b"1")
                    except OSError:
                        return
                    if not chunk or isinstance(chunk, OSError):
                        return

            self.reader = Thread(target=read_pipe, daemon=True)
            self.reader.start()

    def fileno(self) -> int:
        return (self.ready if self.reader is not None else self.stream).fileno()

    def fill(self) -> None:
        if self.reader is not None:
            self.ready.recv(1)
            chunk = self.chunks.get()
            if isinstance(chunk, OSError):
                raise chunk
        elif isinstance(self.stream, socket.socket):
            chunk = self.stream.recv(64 * 1024)
        else:
            chunk = os.read(self.fileno(), 64 * 1024)
        self.buffer.extend(chunk)
        self.eof = not chunk

    def _wait(self, deadline: float) -> None:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([self], [], [], remaining)[0]:
            raise TimeoutError("timed out reading test transport")
        self.fill()

    def readline(self, timeout: float = 15) -> str:
        deadline = time.monotonic() + timeout
        while b"\n" not in self.buffer and not self.eof:
            self._wait(deadline)
        line, separator, remainder = self.buffer.partition(b"\n")
        self.buffer = bytearray(remainder)
        return (line + separator).decode("utf-8")

    def read(self, timeout: float = 15) -> str:
        deadline = time.monotonic() + timeout
        while not self.eof:
            self._wait(deadline)
        result = self.buffer.decode("utf-8")
        self.buffer.clear()
        return result

    def close(self) -> None:
        if self.closed:
            return
        self.closed = True
        if self.reader is not None:
            self.ready.close()
            self.wake.close()
            if self.reader.is_alive():
                import ctypes
                from ctypes import wintypes

                kernel = ctypes.WinDLL("kernel32", use_last_error=True)
                kernel.OpenThread.argtypes = [
                    wintypes.DWORD,
                    wintypes.BOOL,
                    wintypes.DWORD,
                ]
                kernel.OpenThread.restype = wintypes.HANDLE
                kernel.CancelSynchronousIo.argtypes = [wintypes.HANDLE]
                kernel.CloseHandle.argtypes = [wintypes.HANDLE]
                handle = kernel.OpenThread(1, False, self.reader.native_id)
                if handle:
                    try:
                        kernel.CancelSynchronousIo(handle)
                    finally:
                        kernel.CloseHandle(handle)
            self.reader.join(timeout=SERVER_REAP_SECONDS)
            assert not self.reader.is_alive(), "pipe reader did not retire"
        self.stream.close()
        self.eof = True


class McpClient:
    """A synchronous, recording MCP client; use a context to own its lifetime."""

    response_timeout: float = 600
    shutdown_timeout: float = SERVER_SHUTDOWN_SECONDS

    def __init__(
        self,
        binary: Path,
        arguments: tuple[str, ...] = (),
        environment: dict[str, str] | None = None,
        current_directory: Path | None = None,
        umask: int = -1,
        pass_fds: tuple[int, ...] = (),
        response_timeout: float = 600,
        shutdown_timeout: float = SERVER_SHUTDOWN_SECONDS,
        record_in_project: bool = True,
        use_home_configuration: bool = False,
        use_r_startup_files: bool = False,
        recording_receipts: bool = True,
    ) -> None:
        self.response_timeout = response_timeout
        self.shutdown_timeout = shutdown_timeout
        if not use_r_startup_files:
            # Runtime cases preserve HOME and tool caches while excluding the
            # developer's native startup files. Startup cases supply an isolated home.
            environment = {
                **(os.environ if environment is None else environment),
                **dict.fromkeys(
                    ("R_ENVIRON", "R_ENVIRON_USER", "R_PROFILE", "R_PROFILE_USER"),
                    os.devnull,
                ),
            }
        if os.name == "nt" and environment is not None and "TMPDIR" in environment:
            environment = environment | {
                "TEMP": environment["TMPDIR"],
                "TMP": environment["TMPDIR"],
            }
        self.temporary_directory = (
            tempfile.TemporaryDirectory() if current_directory is None else None
        )
        if current_directory is None:
            assert self.temporary_directory is not None
            current_directory = Path(self.temporary_directory.name)
        # Retain CLI fixture paths only for snapshot normalization after assertions.
        workspace = current_directory.resolve()
        self.writable_roots = [
            json.dumps(
                os.path.abspath(workspace / arguments[index + 1]), ensure_ascii=False
            )[1:-1]
            for index, argument in enumerate(arguments)
            if argument == "--writable-root"
        ]
        self.console_home: tempfile.TemporaryDirectory[str] | None = None
        if not use_home_configuration:
            self.console_home = tempfile.TemporaryDirectory()
            environment = {
                **(os.environ if environment is None else environment),
                "MCP_CONSOLE_HOME": self.console_home.name,
            }
        if record_in_project:
            (current_directory / ".agents").mkdir(
                mode=0o700, parents=True, exist_ok=True
            )
            (current_directory / ".agents/console").mkdir(mode=0o700, exist_ok=True)
        process = subprocess.Popen(
            [binary, *arguments],
            env=environment,
            cwd=current_directory,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            umask=umask,
            pass_fds=pass_fds,
        )
        assert process.stdin is not None
        assert process.stdout is not None
        assert process.stderr is not None

        self._track_recording(environment, current_directory, recording_receipts)
        self.process = process
        self.stdin = process.stdin
        self.stdout = TextReader(process.stdout)
        self.stderr = TextReader(process.stderr)
        self.transcript: Transcript = []
        self._next_request_id = 1
        self._issued_request_ids: set[int | str] = set()
        self._last_serialized_message: str | None = None

    def send(self, **arguments: Any) -> ToolResult:
        return self._call_tool("send", **arguments)

    def expect(self, expected: str | None = "[done]", **arguments: Any) -> ToolResult:
        """Collect successful text; None leaves content assertions to the caller."""
        from support.assertions import wait_for_evaluation_output

        wait_for_evaluation_output(
            self,
            expected,
            "expected send output",
            controlled_completion=(
                arguments.get("control") is not None
                and any(
                    arguments.get(language) is not None
                    for language in ("r", "python", "sql")
                )
            ),
            completion_timeout_seconds=self.response_timeout,
            **arguments,
        )
        return self.transcript[-1]["result"]

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *exception: object) -> None:
        self.close()

    def send_message(self, message: dict[str, Any]) -> TranscriptEntry:
        return self.send_messages([message])[0]

    def send_messages(self, messages: list[dict[str, Any]]) -> list[TranscriptEntry]:
        """Record and write multiple complete frames together before any response."""
        assert messages
        entries = []
        serialized = []
        for message in messages:
            recorded_message = message.copy()
            assert recorded_message.pop("jsonrpc", None) == "2.0", message

            entry = {}
            if "id" in recorded_message:
                request_id = recorded_message.pop("id")
                assert isinstance(request_id, (int, str)), message
                assert request_id not in self._issued_request_ids, (
                    f"JSON-RPC request ID was reused: {request_id}"
                )
                self._issued_request_ids.add(request_id)
                entry["id"] = request_id
            params = recorded_message.get("params")
            if (
                recorded_message.keys() == {"method", "params"}
                and recorded_message["method"] == "tools/call"
                and isinstance(params, dict)
                and params.keys() == {"name", "arguments"}
                and params["name"] == "send"
                and isinstance(params["arguments"], dict)
            ):
                entry[params["name"]] = params["arguments"]
            else:
                entry["input"] = recorded_message
            self.transcript.append(entry)
            entries.append(entry)
            serialized.append(json.dumps(message, ensure_ascii=False))
        self._last_serialized_message = serialized[-1]
        self.stdin.write("\n".join(serialized) + "\n")
        self.stdin.flush()
        return entries

    def response_deadline(self) -> float:
        """Bound response and fixture waits while reserving case cleanup time."""
        deadline = time.monotonic() + self.response_timeout
        if (
            case_deadline := os.environ.get("MCP_CONSOLE_TEST_CASE_DEADLINE")
        ) is not None:
            deadline = min(
                deadline, float(case_deadline) - CASE_RESPONSE_RESERVE_SECONDS
            )
        return deadline

    def _read_response_line(self) -> str:
        deadline = self.response_deadline()
        while b"\n" not in self.stdout.buffer and not self.stdout.eof:
            self._wait_for_output(deadline, "response")
        if self.stdout.buffer:
            return self.stdout.readline()
        raise AssertionError(
            f"mcp-console stdout closed before replying: {self._diagnostics()}"
        )

    def _diagnostics(self) -> str:
        tail = self.stderr.buffer[-64 * 1024 :].decode("utf-8", errors="replace")
        return f"return_code={self.process.poll()!r}, stderr={tail!r}"

    def _wait_for_output(self, deadline: float, stage: str) -> None:
        streams = [stream for stream in (self.stdout, self.stderr) if not stream.eof]
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not (
            ready := select.select(streams, [], [], remaining)[0]
        ):
            raise TimeoutError(
                f"mcp-console timed out waiting for {stage}: {self._diagnostics()}"
            )
        for stream in ready:
            stream.fill()

    def receive(self, entry: TranscriptEntry) -> None:
        line = self._read_response_line()
        message = json.loads(line)
        assert message.pop("jsonrpc", None) == "2.0", message
        assert message.pop("id", None) == entry["id"], message
        assert message.keys() == {"result"} or message.keys() == {"error"}, message
        assert entry.keys().isdisjoint(message), message
        entry.update(message)
        self._await_recording(entry)

    def _track_recording(
        self,
        environment: dict[str, str] | None,
        current_directory: Path,
        recording_receipts: bool,
    ) -> None:
        launch = os.environ if environment is None else environment
        self._recording_roots = [
            root / "sessions"
            for root in (
                current_directory / ".agents/console",
                Path(launch["MCP_CONSOLE_HOME"])
                if "MCP_CONSOLE_HOME" in launch
                else Path(launch.get("HOME") or Path.home()) / ".agents/console",
            )
        ]
        # A case clears this before it stalls, stops, or hides the recording.
        # Receipts find the recording by this process's PID, so a case that
        # starts the server through a launcher also clears it.
        self.recording_receipts = recording_receipts
        self._recording_failed = False
        self._recording_published = False
        self._journal_offset = 0
        self._recorded_deliveries: set[int | str] = set()
        self._recorded_receipts: set[int | str] = set()
        self._recorded_assemblies: set[int | str] = set()

    def _await_recording(self, entry: TranscriptEntry) -> None:
        """Wait until the live recording holds this reply's delivery event.

        Replies do not wait for the recording writer. The writer appends a
        reply's delivery event after everything that reply recorded, so a case
        can read the live recording once the event is present. The first send
        publishes the recording; earlier replies have no journal to wait for.
        A recording that the server reports as disabled receives nothing more.
        """
        if not self.recording_receipts:
            return
        request = entry.get("input", {})
        self._recording_published = (
            self._recording_published
            or "send" in entry
            or (
                request.get("method") == "tools/call"
                and isinstance(params := request.get("params"), dict)
                and params.get("name") == "send"
                and isinstance(params.get("arguments"), dict)
            )
        )
        if not self._recording_published:
            return
        self._await_journal(
            self._recorded_deliveries, entry["id"], "the delivery of request"
        )

    def await_received(self, entry: TranscriptEntry) -> None:
        """Wait until the live recording holds the server's receipt of a request.

        The server records the receipt of a message after it has applied every
        earlier message from the same input. A case that holds a reply's write
        cannot use a later reply as that proof, so it waits here instead.
        """
        assert self._await_journal(
            self._recorded_receipts, entry["id"], "the receipt of request"
        ), self._diagnostics()

    def await_assembled(self, entry: TranscriptEntry) -> None:
        """Wait until the live recording holds the server's assembled reply.

        The server records a reply as assembled when its handler has finished
        and before the reply's write starts. A case that holds an earlier
        write waits here to prove that a later handler did not wait for it.
        """
        assert self._await_journal(
            self._recorded_assemblies, entry["id"], "the assembled reply to request"
        ), self._diagnostics()

    def _await_journal(
        self, recorded: set[int | str], request_id: int | str, awaited: str
    ) -> bool:
        """Wait for a journal event; report whether the recording still runs."""
        from support.events import Events

        deadline = self.response_deadline()
        with Events() as events:
            if events.native:
                try:
                    events.watch_process(self.process.pid)
                except ProcessLookupError:
                    return False
            events.watch_readable(self.stderr.fileno())
            # An exited server has finalized or abandoned its recording.
            while self.process.poll() is None:
                # Watch each path before reading it so no append is missed.
                journal = self._watch_journal(events)
                if journal is not None:
                    self._read_journal(journal)
                if request_id in recorded:
                    return True
                if self._recording_disabled():
                    return False
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        f"recording did not receive {awaited} {request_id!r}; "
                        "a case that stalls or hides the recording clears "
                        f"recording_receipts: {self._diagnostics()}"
                    )
                events.wait(remaining)
        return False

    def await_recording_disabled(self) -> None:
        """Wait for the server to report that its recording stopped."""
        deadline = self.response_deadline()
        while not self._recording_disabled():
            self._wait_for_output(deadline, "the recording to stop")

    def _recording_disabled(self) -> bool:
        # The server reports this once, after its last write to the recording.
        while not self.stderr.eof and select.select([self.stderr], [], [], 0)[0]:
            self.stderr.fill()
        if b"transcript recording disabled:" in self.stderr.buffer:
            # A case can opt out of receipts; only this line is the report.
            self._recording_failed = True
            self.recording_receipts = False
        return self._recording_failed

    def _watch_journal(self, events: Any) -> Path | None:
        for root in self._recording_roots:
            events.watch_tree(root, recursive=False)
            for recording in root.glob(f"*-{self.process.pid:010}"):
                journal = recording / "capture/events.jsonl"
                events.watch_tree(journal, recursive=False)
                if journal.exists():
                    return journal
        return None

    def _read_journal(self, journal: Path) -> None:
        try:
            stream = journal.open("rb")
        except PermissionError:
            # Publication copies the journal into place, and a Windows copy
            # holds its destination exclusively. The next pass reads it.
            assert os.name == "nt"
            return
        with stream:
            stream.seek(self._journal_offset)
            appended = stream.read()
        complete = appended.rfind(b"\n") + 1
        self._journal_offset += complete
        for line in appended[:complete].splitlines():
            event = json.loads(line)
            if event["event"] == "delivery":
                self._recorded_deliveries.add(event["request_id"])
            elif event["event"] == "protocol" and "id" in event["message"]:
                if event["direction"] == "received":
                    self._recorded_receipts.add(event["message"]["id"])
                elif event["direction"] == "assembled":
                    self._recorded_assemblies.add(event["message"]["id"])

    def receive_many(self, entries: list[TranscriptEntry]) -> None:
        pending = {entry["id"]: entry for entry in entries}
        assert len(pending) == len(entries), "response batch reused a request ID"
        for _ in entries:
            line = self._read_response_line()
            message = json.loads(line)
            assert message.pop("jsonrpc", None) == "2.0", message
            request_id = message.pop("id", None)
            assert request_id in pending, message
            entry = pending.pop(request_id)
            assert message.keys() == {"result"} or message.keys() == {"error"}, message
            assert entry.keys().isdisjoint(message), message
            entry.update(message)
        for entry in entries:
            self._await_recording(entry)

    def start_request(self, method: str, **params: Any) -> TranscriptEntry:
        message: dict[str, Any] = {
            "jsonrpc": "2.0",
            "id": self._next_request_id,
            "method": method,
        }
        self._next_request_id += 1
        if params:
            message["params"] = params

        return self.send_message(message)

    def request(self, method: str, **params: Any) -> TranscriptEntry:
        entry = self.start_request(method, **params)
        self.receive(entry)
        return entry

    def notify(self, method: str, **params: Any) -> None:
        message: dict[str, Any] = {
            "jsonrpc": "2.0",
            "method": method,
        }
        if params:
            message["params"] = params

        self.send_message(message)

    def initialize_and_list_tools(self) -> None:
        self.request(
            "initialize",
            protocolVersion="2025-11-25",
            capabilities={},
            clientInfo={
                "name": "acceptance-test",
                "version": "1.0.0",
            },
        )
        self.notify("notifications/initialized")
        self.request("tools/list")

    def startup_error(self) -> str:
        """Complete MCP discovery and observe a failed runtime through send."""
        self.initialize_and_list_tools()
        result = self.send(requirements={"action": "get"})
        assert result["isError"], result
        assert all(part["type"] == "text" for part in result["content"]), result
        return "".join(part["text"] for part in result["content"])

    def _start_tool_call(self, name: str, **arguments: Any) -> TranscriptEntry:
        return self.start_request(
            "tools/call",
            name=name,
            arguments=arguments,
        )

    def _call_tool(self, name: str, **arguments: Any) -> ToolResult:
        entry = self._start_tool_call(name, **arguments)
        self.receive(entry)
        result = entry["result"]
        assert isinstance(result, dict), result
        return result

    def start_send(self, **arguments: Any) -> TranscriptEntry:
        return self._start_tool_call("send", **arguments)

    def finish(self) -> Transcript:
        transcript, standard_error = self.finish_with_standard_error()
        assert standard_error == "", standard_error
        return transcript

    def finish_with_standard_error(
        self, *, expected_exit_status: int = 0
    ) -> tuple[Transcript, str]:
        deadline = self._cleanup_deadline()
        try:
            self._shutdown(deadline - SERVER_REAP_SECONDS)
            extra_output = self.stdout.read()
            standard_error = self.stderr.read()
            assert self.process.returncode == expected_exit_status, standard_error
            assert extra_output == "", f"unexpected extra output: {extra_output}"
            for entry in self.transcript:
                if isinstance(result := entry.get("result"), dict):
                    for tool in result.get("tools", []):
                        for index, root in enumerate(self.writable_roots):
                            label = (
                                "<writable-root>"
                                if index == 0
                                else f"<writable-root {index + 1}>"
                            )
                            tool["description"] = tool["description"].replace(
                                f'`"{root}"`', f'`"{label}"`'
                            )
            return self.transcript, standard_error
        finally:
            self._dispose(deadline)

    def _cleanup_deadline(self) -> float:
        timeout = self.shutdown_timeout
        if "MCP_CONSOLE_TEST_CASE_DEADLINE" in os.environ:
            timeout = min(timeout, SERVER_SHUTDOWN_SECONDS)
        # Normal server retirement has up to ten seconds of staged deadlines.
        # Reserve PID kill/reap time inside the supervisor's fifteen seconds.
        return time.monotonic() + timeout + SERVER_REAP_SECONDS

    def _shutdown(self, deadline: float) -> None:
        if not self.stdin.closed:
            self.stdin.close()
        while not (self.stdout.eof and self.stderr.eof):
            self._wait_for_output(deadline, "shutdown")
        try:
            self.process.wait(timeout=max(0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            raise TimeoutError(
                f"mcp-console timed out waiting for shutdown: {self._diagnostics()}"
            ) from None

    def _dispose(self, deadline: float) -> None:
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(
            timeout=min(SERVER_REAP_SECONDS, max(0, deadline - time.monotonic()))
        )
        for stream in (self.stdin, self.stdout, self.stderr):
            stream.close()
        if self.temporary_directory is not None:
            self.temporary_directory.cleanup()
        if self.console_home is not None:
            self.console_home.cleanup()

    def close(self) -> None:
        """Close input, allow staged retirement, then kill only the server PID."""
        if (
            self.stdout.closed
            and self.stderr.closed
            and self.process.poll() is not None
        ):
            return
        deadline = self._cleanup_deadline()
        try:
            self._shutdown(deadline - SERVER_REAP_SECONDS)
        except TimeoutError:
            pass
        finally:
            self._dispose(deadline)


def stop_client(client: McpClient) -> None:
    client.close()
