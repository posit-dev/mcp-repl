#!/usr/bin/env python3
import base64
import json
import os
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

CHUNK_SIZE = 65536
DEBUG_DIR_ENV = "MCP_REPL_DEBUG_DIR"
DEBUG_SESSION_DIR_ENV = "MCP_REPL_DEBUG_SESSION_DIR"
FORWARD_STDERR_ENV = "MCP_REPL_TRACE_FORWARD_STDERR"
STREAM_META = {
    "stdin": {"route": "mcp_client -> mcp_server"},
    "stdout": {"route": "mcp_server -> mcp_client"},
    "stderr": {"route": "mcp_server_stderr -> trace_log"},
}


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


class Logger:
    def __init__(self, raw_path: Path, pretty_path: Path):
        self._lock = threading.Lock()
        self.raw_file = raw_path.open("a", encoding="utf-8")
        self.pretty_file = pretty_path.open("a", encoding="utf-8")

    def write(self, event: str, **payload):
        record = {
            "ts_unix_ms": now_ms(),
            "pid": os.getpid(),
            "event": event,
            "payload": payload,
        }
        with self._lock:
            self.raw_file.write(json.dumps(record, ensure_ascii=False))
            self.raw_file.write("\n")
            self.raw_file.flush()

            self.pretty_file.write(json.dumps(record, ensure_ascii=False, indent=2))
            self.pretty_file.write("\n\n")
            self.pretty_file.flush()


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


def log_chunk(log: Logger, stream: str, chunk: bytes):
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


def forward_stream(log: Logger, stream: str, src, dst=None, forward_stderr=False):
    src_fd = src.fileno()
    dst_fd = dst.fileno() if dst is not None else None
    stderr_fd = sys.stderr.fileno() if stream == "stderr" and forward_stderr else None

    while True:
        chunk = os.read(src_fd, CHUNK_SIZE)
        if not chunk:
            log.write("stream_closed", stream=stream)
            if stream == "stdin" and dst is not None:
                try:
                    dst.close()
                except Exception:
                    pass
            return

        log_chunk(log, stream, chunk)
        if dst_fd is not None:
            try:
                write_all(dst_fd, chunk)
            except OSError:
                log.write("stream_broken_pipe", stream=stream)
                return
        elif stderr_fd is not None:
            try:
                write_all(stderr_fd, chunk)
            except OSError:
                log.write("stream_broken_pipe", stream=stream)
                return


def main():
    if len(sys.argv) < 2:
        print("usage: codex-stdio-trace-win REAL_MCP_SERVER [ARGS...]", file=sys.stderr)
        return 2

    real_cmd = sys.argv[1:]
    debug_root = Path(os.environ.get(DEBUG_DIR_ENV, Path.cwd() / ".mcp-repl-debug"))
    session_dir = make_session_dir(debug_root)
    raw_path = session_dir / "wire.jsonl"
    pretty_path = session_dir / "wire.pretty.json"
    log = Logger(raw_path, pretty_path)

    child_env = os.environ.copy()
    child_env[DEBUG_SESSION_DIR_ENV] = str(session_dir)
    child_env[DEBUG_DIR_ENV] = str(debug_root)

    log.write(
        "startup",
        cwd=str(Path.cwd()),
        argv=sys.argv,
        real_cmd=real_cmd,
        session_dir=str(session_dir),
        log_path=str(raw_path),
        pretty_log_path=str(pretty_path),
        forward_stderr=bool(os.environ.get(FORWARD_STDERR_ENV)),
    )

    child = subprocess.Popen(
        real_cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
        env=child_env,
    )
    log.write("child_spawned", child_pid=child.pid)

    def forward_signal(signum, _frame):
        log.write("signal_forward", signal=signum)
        try:
            child.send_signal(signum)
        except Exception as exc:
            log.write("signal_forward_error", signal=signum, error=repr(exc))

    for name in ("SIGINT", "SIGTERM", "SIGHUP"):
        sig = getattr(signal, name, None)
        if sig is None:
            continue
        try:
            signal.signal(sig, forward_signal)
        except Exception:
            pass

    threads = [
        threading.Thread(
            target=forward_stream,
            args=(log, "stdin", sys.stdin.buffer, child.stdin, False),
            daemon=True,
        ),
        threading.Thread(
            target=forward_stream,
            args=(log, "stdout", child.stdout, sys.stdout.buffer, False),
            daemon=True,
        ),
        threading.Thread(
            target=forward_stream,
            args=(
                log,
                "stderr",
                child.stderr,
                None,
                bool(os.environ.get(FORWARD_STDERR_ENV)),
            ),
            daemon=True,
        ),
    ]

    for thread in threads:
        thread.start()

    exit_code = child.wait()
    log.write("child_exit", exit_code=exit_code)

    for thread in threads:
        thread.join(timeout=1.0)

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
