#!/usr/bin/env python3
import base64
import json
import os
import selectors
import signal
import subprocess
import sys
import time
from pathlib import Path

CHUNK_SIZE = 65536
LOG_DIR_NAME = ".mcp-repl-trace"
FORWARD_STDERR_ENV = "MCP_REPL_TRACE_FORWARD_STDERR"
DEBUG_DIR_ENV = "MCP_REPL_DEBUG_DIR"
DEBUG_SESSION_DIR_ENV = "MCP_REPL_DEBUG_SESSION_DIR"
STREAM_META = {
    "stdin": {"route": "mcp_client -> mcp_server"},
    "stdout": {"route": "mcp_server -> mcp_client"},
    "stderr": {"route": "mcp_server_stderr -> trace_log"},
}


def filtered_prefixed_env(prefixes: tuple[str, ...]):
    out = {}
    for k, v in os.environ.items():
        if not any(k.startswith(prefix) for prefix in prefixes):
            continue
        upper = k.upper()
        if any(token in upper for token in ("KEY", "TOKEN", "SECRET", "PASSWORD")):
            continue
        out[k] = v
    return dict(sorted(out.items()))


def now_ms():
    return time.time_ns() // 1_000_000


def make_session_dir(base_dir: Path) -> Path:
    base_dir.mkdir(parents=True, exist_ok=True)
    stem = f"session-{now_ms()}-{os.getpid()}"
    for suffix in range(1000):
        name = stem if suffix == 0 else f"{stem}-{suffix}"
        session_dir = base_dir / name
        try:
            session_dir.mkdir(parents=False, exist_ok=False)
            return session_dir
        except FileExistsError:
            continue
    raise RuntimeError("failed to allocate unique debug session directory")


def resolve_log_dir() -> Path:
    session_dir = os.environ.get(DEBUG_SESSION_DIR_ENV)
    if session_dir:
        path = Path(session_dir)
        path.mkdir(parents=True, exist_ok=True)
        return path

    debug_dir = os.environ.get(DEBUG_DIR_ENV)
    if debug_dir:
        path = make_session_dir(Path(debug_dir))
        os.environ[DEBUG_SESSION_DIR_ENV] = str(path)
        return path

    path = Path.cwd() / LOG_DIR_NAME
    path.mkdir(parents=True, exist_ok=True)
    return path


def make_log_paths() -> tuple[Path, Path]:
    log_dir = resolve_log_dir()
    if log_dir.name.startswith("session-"):
        return log_dir / "wire.jsonl", log_dir / "wire.pretty.json"
    stem = f"trace-{now_ms()}-{os.getpid()}"
    return log_dir / f"{stem}.jsonl", log_dir / f"{stem}.pretty.json"


class Logger:
    def __init__(self, raw_path: Path, pretty_path: Path):
        self.raw_file = raw_path.open("a", encoding="utf-8")
        self.pretty_file = pretty_path.open("a", encoding="utf-8")
        self.raw_path = raw_path
        self.pretty_path = pretty_path

    def write(self, event: str, **payload):
        record = {
            "ts_unix_ms": now_ms(),
            "pid": os.getpid(),
            "event": event,
            "payload": payload,
        }
        self.raw_file.write(json.dumps(record, ensure_ascii=False))
        self.raw_file.write("\n")
        self.raw_file.flush()

        self.pretty_file.write(json.dumps(record, ensure_ascii=False, indent=2))
        self.pretty_file.write("\n\n")
        self.pretty_file.flush()


log = None
child = None


def decode_chunk(chunk: bytes):
    payload = {}
    try:
        text = chunk.decode("utf-8")
    except UnicodeDecodeError:
        return payload

    payload["text"] = text
    lines = [line for line in text.splitlines() if line.strip()]
    if not lines:
        return payload

    parsed = []
    for line in lines:
        try:
            parsed.append(json.loads(line))
        except json.JSONDecodeError:
            return payload

    payload["text_as_json"] = parsed[0] if len(parsed) == 1 else parsed
    return payload


def log_chunk(stream: str, chunk: bytes):
    payload = {
        "stream": stream,
        **STREAM_META[stream],
        "size": len(chunk),
        "data_b64": base64.b64encode(chunk).decode("ascii"),
    }
    payload.update(decode_chunk(chunk))
    log.write("stream_chunk", **payload)


def write_all(fd: int, chunk: bytes):
    view = memoryview(chunk)
    while view:
        written = os.write(fd, view)
        view = view[written:]


def forward_signal(signum, _frame):
    log.write("signal_forward", signal=signum)
    if child is not None and child.poll() is None:
        try:
            child.send_signal(signum)
        except Exception as exc:
            log.write("signal_forward_error", signal=signum, error=repr(exc))


def main():
    global log
    global child

    if len(sys.argv) < 2:
        print("usage: mcp-stdio-trace REAL_MCP_SERVER [ARGS...]", file=sys.stderr)
        return 2

    real_cmd = sys.argv[1:]
    raw_path, pretty_path = make_log_paths()
    log = Logger(raw_path, pretty_path)
    log.write(
        "startup",
        cwd=str(Path.cwd()),
        argv=sys.argv,
        real_cmd=real_cmd,
        log_path=str(raw_path),
        pretty_log_path=str(pretty_path),
        forward_stderr=bool(os.environ.get(FORWARD_STDERR_ENV)),
        ppid=os.getppid(),
        visible_env=filtered_prefixed_env(("MCP_", "CODEX_", "CLAUDE_")),
    )

    child_env = os.environ.copy()
    child = subprocess.Popen(
        real_cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
        env=child_env,
    )
    log.write("child_spawned", child_pid=child.pid)

    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        try:
            signal.signal(sig, forward_signal)
        except Exception:
            pass

    selector = selectors.DefaultSelector()
    stdin_fd = sys.stdin.buffer.raw.fileno()
    child_stdin_fd = child.stdin.fileno()
    child_stdout_fd = child.stdout.fileno()
    child_stderr_fd = child.stderr.fileno()
    stdout_fd = sys.stdout.buffer.raw.fileno()
    stderr_fd = sys.stderr.buffer.raw.fileno()
    forward_stderr = bool(os.environ.get(FORWARD_STDERR_ENV))

    selector.register(stdin_fd, selectors.EVENT_READ, "stdin")
    selector.register(child_stdout_fd, selectors.EVENT_READ, "stdout")
    selector.register(child_stderr_fd, selectors.EVENT_READ, "stderr")

    stdin_open = True
    open_streams = {"stdout", "stderr"}

    while True:
        if child.poll() is not None and not open_streams:
            break
        events = selector.select(timeout=0.25)
        if not events:
            continue
        for key, _mask in events:
            stream = key.data
            try:
                chunk = os.read(key.fd, CHUNK_SIZE)
            except OSError as exc:
                log.write("stream_error", stream=stream, error=repr(exc))
                chunk = b""

            if chunk:
                log_chunk(stream, chunk)
                if stream == "stdin":
                    try:
                        write_all(child_stdin_fd, chunk)
                    except BrokenPipeError:
                        log.write("stream_broken_pipe", stream=stream)
                        try:
                            selector.unregister(stdin_fd)
                        except Exception:
                            pass
                        stdin_open = False
                    except OSError as exc:
                        log.write("stream_error", stream=stream, error=repr(exc))
                        try:
                            selector.unregister(stdin_fd)
                        except Exception:
                            pass
                        stdin_open = False
                elif stream == "stdout":
                    write_all(stdout_fd, chunk)
                elif stream == "stderr" and forward_stderr:
                    write_all(stderr_fd, chunk)
                continue

            try:
                selector.unregister(key.fd)
            except Exception:
                pass

            if stream == "stdin" and stdin_open:
                stdin_open = False
                try:
                    child.stdin.close()
                except Exception:
                    pass
            else:
                open_streams.discard(stream)
            log.write("stream_closed", stream=stream)

    exit_code = child.wait()
    log.write("child_exit", exit_code=exit_code)
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
