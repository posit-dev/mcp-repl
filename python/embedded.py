import base64
import builtins
import hashlib
import importlib.util
import io
import os
import pydoc
import sys
import threading

import _mcp_repl

os.environ.setdefault("MPLBACKEND", "agg")
sys.argv = ["mcp-repl"]
_executable = _mcp_repl.executable()
if _executable:
    sys.executable = _executable
    sys._base_executable = _executable

_plot_capable = importlib.util.find_spec("matplotlib") is not None
_plot_modules_loaded = False
_plot_pyplot = None
_plot_known_figures = set()
_plot_hashes = {}
_plot_lock = threading.Lock()
_plot_hooks_installed = False
_plot_emit_in_progress = False
_plot_axes_plot = None
_plot_show = None
_plot_emitted_this_request = {}


def _input(prompt=""):
    prompt = str(prompt)
    if prompt:
        sys.stdout.write(prompt)
        sys.stdout.flush()
    line = _mcp_repl.readline(prompt)
    if line is None:
        raise EOFError
    if line.endswith("\n"):
        line = line[:-1]
        if line.endswith("\r"):
            line = line[:-1]
    return line


def _pydoc_plainpager(text, title=""):
    pydoc.plainpager(text)


class McpInputStream:
    encoding = "utf-8"
    errors = "replace"
    closed = False

    def readline(self, size=-1):
        line = _mcp_repl.readline("")
        return "" if line is None else line

    def readable(self):
        return True

    def isatty(self):
        return False

    def fileno(self):
        return 0

    def close(self):
        pass

    def flush(self):
        pass


class McpOutputStream:
    encoding = "utf-8"
    errors = "replace"
    closed = False

    def __init__(self, stream):
        self._stream = stream

    def write(self, message):
        return _mcp_repl.write(self._stream, str(message))

    def flush(self):
        pass

    def writable(self):
        return True

    def isatty(self):
        return True

    def fileno(self):
        return 1 if self._stream == "stdout" else 2

    def close(self):
        pass


def _ensure_plot_modules():
    global _plot_modules_loaded, _plot_pyplot, _plot_capable
    global _plot_hooks_installed, _plot_axes_plot, _plot_show

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
                fig_num = getattr(getattr(self, "figure", None), "number", None)
                force_figures = {fig_num} if fig_num is not None else None
                _maybe_emit_plots(force_figures=force_figures)
                return result

            Axes.plot = _wrapped_plot
            _plot_show = plt.show

            def _wrapped_show(*args, **kwargs):
                result = _plot_show(*args, **kwargs)
                _maybe_emit_plots(force_all=True)
                return result

            plt.show = _wrapped_show
            _plot_hooks_installed = True
        _plot_modules_loaded = True
        return True
    except Exception:
        _plot_capable = False
        return False


def _maybe_emit_plots(force_figures=None, force_all=False):
    if not _mcp_repl.has_request_active():
        return
    _emit_plots(force_figures=force_figures, force_all=force_all)


def _emit_plots(force_figures=None, force_all=False):
    global _plot_known_figures, _plot_hashes, _plot_emitted_this_request
    global _plot_emit_in_progress

    if _mcp_repl.take_plot_reset_pending():
        with _plot_lock:
            _plot_emitted_this_request = {}

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
            _plot_hashes = {}
            _plot_emitted_this_request = {}
        _plot_emit_in_progress = False
        return

    fig_nums = sorted(fig_nums)
    new_known = set(fig_nums)
    force_figures = set() if force_figures is None else set(force_figures)
    with _plot_lock:
        prev_known = set(_plot_known_figures)
        for stale_num in set(_plot_hashes) - new_known:
            _plot_hashes.pop(stale_num, None)
            _plot_emitted_this_request.pop(stale_num, None)

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
        force_current = force_all or fig_num in force_figures
        with _plot_lock:
            emitted_this_request = _plot_emitted_this_request.get(fig_num) == digest
            if emitted_this_request:
                continue
            if _plot_hashes.get(fig_num) == digest and not force_current:
                continue
            _plot_hashes[fig_num] = digest
            _plot_emitted_this_request[fig_num] = digest

        encoded = base64.b64encode(data).decode("ascii")
        is_new = fig_num not in prev_known
        _mcp_repl.emit_plot_image("image/png", encoded, not bool(is_new), str(fig_num))

    with _plot_lock:
        _plot_known_figures = new_known
    _plot_emit_in_progress = False


def _mcp_repl_begin_request():
    global _plot_emitted_this_request
    with _plot_lock:
        _plot_emitted_this_request = {}


def _mcp_repl_emit_plots():
    _emit_plots()


def _mcp_repl_plot_capable():
    return bool(_plot_capable)


builtins.input = _input
pydoc.pager = _pydoc_plainpager
sys.ps1 = ""
sys.ps2 = ""
sys.stdin = McpInputStream()
sys.stdout = McpOutputStream("stdout")
sys.stderr = McpOutputStream("stderr")
