import base64
import builtins
import hashlib
import importlib.util
import io
import json
import os
import readline
import signal
import select
import sys
import threading

os.environ.setdefault("MPLBACKEND", "agg")

_read_fd_value = os.environ.get("MCP_REPL_IPC_READ_FD")
_write_fd_value = os.environ.get("MCP_REPL_IPC_WRITE_FD")
if not _read_fd_value or not _write_fd_value:
    raise SystemExit("MCP_REPL_IPC_READ_FD or MCP_REPL_IPC_WRITE_FD missing")

_ipc_read_fd = int(_read_fd_value)
_ipc_write_fd = int(_write_fd_value)
# These env vars are a one-shot bootstrap path for the main worker. Child processes must not
# inherit the live IPC fds or discover them later through the environment.
os.set_inheritable(_ipc_read_fd, False)
os.set_inheritable(_ipc_write_fd, False)
os.environ.pop("MCP_REPL_IPC_READ_FD", None)
os.environ.pop("MCP_REPL_IPC_WRITE_FD", None)
_ipc_read = os.fdopen(_ipc_read_fd, "r", buffering=1)
_ipc_write = os.fdopen(_ipc_write_fd, "w", buffering=1)
_ipc_lock = threading.Lock()
_request_lock = threading.Lock()
_request_active = False
_interrupt_pending = False
_input_prompt_active = False
_suppress_next_pre_input = False
_last_prompt = None
_primary_prompt = None
_continuation_prompt = None
_ipc_ok = True
_plot_capable = importlib.util.find_spec("matplotlib") is not None
_plot_modules_loaded = False
_plot_pyplot = None
_plot_known_figures = set()
_plot_hashes = {}
_plot_lock = threading.Lock()
_plot_pid = os.getpid()
_plot_hooks_installed = False
_plot_emit_in_progress = False
_plot_axes_plot = None
_plot_show = None


def _close_ipc_in_fork_child():
    global _ipc_ok
    _ipc_ok = False
    # The child only needs the inherited descriptors gone. Closing the buffered wrappers can
    # deadlock on inherited _io locks from threads that vanished across fork.
    for fd in (_ipc_read_fd, _ipc_write_fd):
        try:
            os.close(fd)
        except Exception:
            pass


try:
    os.register_at_fork(after_in_child=_close_ipc_in_fork_child)
except AttributeError:
    pass


def _send(obj):
    global _ipc_ok
    if not _ipc_ok:
        return
    try:
        payload = json.dumps(obj)
        with _ipc_lock:
            _ipc_write.write(payload + "\n")
            _ipc_write.flush()
    except Exception:
        _ipc_ok = False
        try:
            readline.set_pre_input_hook(None)
        except Exception:
            pass


def _ensure_plot_modules():
    global _plot_modules_loaded, _plot_pyplot, _plot_capable, _plot_hooks_installed, _plot_axes_plot, _plot_show
    if not _plot_capable:
        return False
    if _plot_modules_loaded:
        return True
    try:
        import matplotlib

        if "matplotlib.pyplot" not in sys.modules:
            matplotlib.use("agg", force=True)
        import matplotlib.pyplot as plt

        _plot_pyplot = plt
        if not _plot_hooks_installed:
            from matplotlib.axes import Axes

            _plot_axes_plot = Axes.plot

            def _wrapped_plot(self, *args, **kwargs):
                result = _plot_axes_plot(self, *args, **kwargs)
                _maybe_emit_plots()
                return result

            Axes.plot = _wrapped_plot
            _plot_show = plt.show

            def _wrapped_show(*args, **kwargs):
                result = _plot_show(*args, **kwargs)
                _maybe_emit_plots()
                return result

            plt.show = _wrapped_show
            _plot_hooks_installed = True
        _plot_modules_loaded = True
        return True
    except Exception:
        _plot_capable = False
        return False


def _reset_plot_hashes():
    global _plot_hashes
    if not _plot_capable:
        return
    with _plot_lock:
        _plot_hashes = {}


def _maybe_emit_plots():
    if not _has_request_active():
        return
    _emit_plots()


def _emit_plots():
    global _plot_known_figures, _plot_hashes, _plot_emit_in_progress
    if not _plot_capable:
        return
    if _plot_emit_in_progress:
        return
    if "matplotlib.pyplot" not in sys.modules and "matplotlib" not in sys.modules:
        return
    if not _ensure_plot_modules():
        return
    try:
        import matplotlib.pyplot as plt
    except Exception:
        return
    _plot_emit_in_progress = True
    try:
        fig_nums = plt.get_fignums()
    except Exception:
        _plot_emit_in_progress = False
        return
    if not fig_nums:
        with _plot_lock:
            _plot_known_figures = set()
        _plot_emit_in_progress = False
        return
    fig_nums = sorted(fig_nums)
    with _plot_lock:
        prev_known = set(_plot_known_figures)
    new_known = set(fig_nums)
    for fig_num in fig_nums:
        try:
            fig = plt.figure(fig_num)
            buf = io.BytesIO()
            fig.savefig(buf, format="png")
            data = buf.getvalue()
            buf.close()
        except Exception:
            continue
        digest = hashlib.sha256(data).hexdigest()
        with _plot_lock:
            if _plot_hashes.get(fig_num) == digest:
                continue
            _plot_hashes[fig_num] = digest
        encoded = base64.b64encode(data).decode("ascii")
        is_new = fig_num not in prev_known
        _send(
            {
                "type": "plot_image",
                "id": f"plot-{_plot_pid}-{fig_num}",
                "mime_type": "image/png",
                "data": encoded,
                "is_new": bool(is_new),
            }
        )
    with _plot_lock:
        _plot_known_figures = new_known
    _plot_emit_in_progress = False


def _set_request_active():
    global _request_active, _interrupt_pending
    with _request_lock:
        _request_active = True
        _interrupt_pending = False


def _take_request_active():
    global _request_active, _interrupt_pending
    with _request_lock:
        was_active = _request_active
        _request_active = False
        _interrupt_pending = False
    return was_active


def _has_request_active():
    with _request_lock:
        return _request_active


def _emit_backend_info():
    _send(
        {
            "type": "backend_info",
            "language": "python",
            "supports_images": _plot_capable,
        }
    )


class _Prompt(str):
    def __new__(cls, value, emit):
        obj = super().__new__(cls, value)
        obj.emit = emit
        return obj

    def __str__(self):
        global _last_prompt
        _last_prompt = super().__str__()
        # Emit prompt + request_end even if readline pre_input_hook is not invoked (e.g. after some
        # interrupts). Only emit for the primary prompt; continuation prompts are handled by the
        # pre_input hook and emitting from sys.ps2 can interfere with prompt selection.
        if self.emit:
            _emit_prompt(_last_prompt)
        return _last_prompt


def _ensure_prompts():
    global _primary_prompt, _continuation_prompt, _last_prompt
    ps1 = getattr(sys, "ps1", ">>> ")
    ps2 = getattr(sys, "ps2", "... ")
    _primary_prompt = str(ps1)
    _continuation_prompt = str(ps2)
    sys.ps1 = _Prompt(_primary_prompt, True)
    sys.ps2 = _Prompt(_continuation_prompt, False)
    _last_prompt = _primary_prompt


def _stdin_has_data():
    # We only want to end a request when Python is about to block waiting on input.
    # When the server sends a multi-line chunk, the interpreter may produce prompts
    # between internal reads while stdin still has buffered data. In that case we
    # must NOT emit request_end yet.
    try:
        readable, _, _ = select.select([0], [], [], 0)
        return bool(readable)
    except Exception:
        return False


def _run_with_sigint_blocked(fn):
    pthread_sigmask = getattr(signal, "pthread_sigmask", None)
    if pthread_sigmask is None:
        return fn()
    try:
        previous = pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT})
    except Exception:
        return fn()
    try:
        return fn()
    finally:
        try:
            pthread_sigmask(signal.SIG_SETMASK, previous)
        except Exception:
            pass


def _drain_stdin_nonblocking():
    previous_blocking = None
    try:
        previous_blocking = os.get_blocking(0)
        os.set_blocking(0, False)
    except Exception:
        previous_blocking = None

    try:
        while True:
            try:
                readable, _, _ = select.select([0], [], [], 0)
            except Exception:
                return
            if not readable:
                return
            try:
                chunk = os.read(0, 65536)
            except BlockingIOError:
                return
            except Exception:
                return
            if not chunk:
                return
    finally:
        if previous_blocking is not None:
            try:
                os.set_blocking(0, previous_blocking)
            except Exception:
                pass


def _discard_pending_request_input():
    global _interrupt_pending
    with _request_lock:
        interrupt_pending = _interrupt_pending
        request_active = _request_active

    if not interrupt_pending:
        return
    if not request_active:
        with _request_lock:
            _interrupt_pending = False
        return

    def _drain_until_quiet():
        for _ in range(8):
            _drain_stdin_nonblocking()
            if not _stdin_has_data():
                return

    try:
        _run_with_sigint_blocked(_drain_until_quiet)
    except KeyboardInterrupt:
        # Keep default runtime interrupt behavior if SIGINT lands during drain.
        return

    if not _stdin_has_data():
        with _request_lock:
            _interrupt_pending = False


def _emit_prompt(prompt=None, emit_request_end=True):
    _discard_pending_request_input()
    if prompt is None:
        prompt = _last_prompt or _primary_prompt or getattr(sys, "ps1", ">>> ")
    _send({"type": "readline_start", "prompt": str(prompt)})
    if not emit_request_end:
        return
    if not _has_request_active():
        return
    if _stdin_has_data():
        return
    _emit_plots()
    if not _take_request_active():
        return
    # Best-effort: ensure the client observes all output written for the request before the
    # request_end event. The Rust side uses a short "settle" window after request_end too.
    try:
        sys.stdout.flush()
        sys.stderr.flush()
    except Exception:
        pass
    _send({"type": "request_end"})


def _pre_input_hook():
    global _suppress_next_pre_input
    if _suppress_next_pre_input:
        _suppress_next_pre_input = False
        return
    _emit_prompt()


def _wrap_input():
    orig = builtins.input

    def _input(prompt=""):
        global _input_prompt_active, _suppress_next_pre_input, _last_prompt
        _input_prompt_active = True
        _last_prompt = str(prompt)
        _suppress_next_pre_input = True
        _emit_prompt(_last_prompt)
        try:
            line = orig(prompt)
        except Exception:
            _input_prompt_active = False
            _suppress_next_pre_input = False
            raise
        _input_prompt_active = False
        # Preserve line-ending semantics expected by the server's echo attribution:
        # `line` includes the consumed newline when present.
        consumed = line + "\n"
        _send(
            {
                "type": "readline_result",
                "prompt": str(prompt),
                "line": consumed,
            }
        )
        return line

    builtins.input = _input


def _ipc_reader():
    global _interrupt_pending
    for raw in _ipc_read:
        if not raw:
            break
        try:
            msg = json.loads(raw)
        except Exception:
            continue
        msg_type = msg.get("type")
        if msg_type == "stdin_write":
            _set_request_active()
            _reset_plot_hashes()
        elif msg_type == "interrupt":
            with _request_lock:
                _interrupt_pending = True


_emit_backend_info()
threading.Thread(target=_ipc_reader, daemon=True).start()
_ensure_prompts()
_wrap_input()
readline.set_pre_input_hook(_pre_input_hook)
