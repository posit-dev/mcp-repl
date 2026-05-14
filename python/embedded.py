import base64
import builtins
import errno
import hashlib
import importlib.util
import io
import operator
import os
import pydoc
import sys
import threading

import _io
import _mcp_repl

try:
    import posix as _mcp_repl_posix
except ImportError:
    _mcp_repl_posix = None

os.environ.setdefault("MPLBACKEND", "agg")
sys.argv = ["mcp-repl"]
if "" not in sys.path:
    sys.path.insert(0, "")

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
_mcp_repl_ps1 = ">>> "
_mcp_repl_ps2 = "... "


class _McpSuppressedPrompt:
    def __init__(self, prompt_name):
        self._prompt_name = prompt_name

    def __str__(self):
        return ""

    def __repr__(self):
        if self._prompt_name == "ps1":
            return repr(_mcp_repl_ps1)
        return repr(_mcp_repl_ps2)


_mcp_repl_suppressed_ps1 = _McpSuppressedPrompt("ps1")
_mcp_repl_suppressed_ps2 = _McpSuppressedPrompt("ps2")


def _mcp_repl_capture_prompts():
    global _mcp_repl_ps1, _mcp_repl_ps2
    ps1 = getattr(sys, "ps1", _mcp_repl_suppressed_ps1)
    ps2 = getattr(sys, "ps2", _mcp_repl_suppressed_ps2)
    if ps1 is not _mcp_repl_suppressed_ps1:
        _mcp_repl_ps1 = str(ps1)
    if ps2 is not _mcp_repl_suppressed_ps2:
        _mcp_repl_ps2 = str(ps2)
    _mcp_repl.set_python_prompts(_mcp_repl_ps1, _mcp_repl_ps2)
    sys.ps1 = _mcp_repl_suppressed_ps1
    sys.ps2 = _mcp_repl_suppressed_ps2


def _input(prompt=""):
    prompt = str(prompt)
    stdin = sys.stdin
    if isinstance(stdin, McpInputStream):
        line = stdin._readline_for_input(prompt)
    else:
        if prompt:
            sys.stdout.write(prompt)
            sys.stdout.flush()
        line = stdin.readline()
    if line == "":
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
    newlines = None

    def __init__(self, fileno=0, closefd=False):
        self._buffer = b""
        self._fileno = fileno
        self._closefd = closefd
        self.buffer = McpInputBuffer(self)
        self.closed = False

    def _check_open(self):
        if self.closed:
            raise ValueError("I/O operation on closed file.")

    def _normalize_size(self, size):
        if size is None:
            return -1
        return int(size)

    def _read_backend_line(self, prompt=""):
        line = _mcp_repl.readline(prompt)
        if line is None:
            return None
        return line.encode(self.encoding)

    def _emit_prompt(self, prompt):
        if prompt:
            _mcp_repl.write("stdout", prompt)

    def _take_buffered(self, size):
        chunk = self._buffer[:size]
        self._buffer = self._buffer[size:]
        return chunk

    def _read_bytes(self, size=-1):
        self._check_open()
        size = self._normalize_size(size)
        if size == 0:
            return b""
        if size < 0:
            chunks = [self._buffer]
            self._buffer = b""
            while True:
                line = self._read_backend_line()
                if line is None:
                    break
                chunks.append(line)
            return b"".join(chunks)

        chunks = []
        remaining = size
        if self._buffer:
            chunk = self._take_buffered(remaining)
            chunks.append(chunk)
            remaining -= len(chunk)
        while remaining > 0:
            line = self._read_backend_line()
            if line is None:
                break
            chunks.append(line[:remaining])
            if len(line) > remaining:
                self._buffer = line[remaining:] + self._buffer
            remaining -= len(chunks[-1])
        return b"".join(chunks)

    def _readline_bytes(self, size=-1, prompt=""):
        self._check_open()
        size = self._normalize_size(size)
        if size == 0:
            return b""

        newline_index = self._buffer.find(b"\n")
        if newline_index >= 0:
            self._emit_prompt(prompt)
            end = newline_index + 1
            if size > 0:
                end = min(end, size)
            return self._take_buffered(end)
        if size > 0 and len(self._buffer) >= size:
            self._emit_prompt(prompt)
            return self._take_buffered(size)

        line = self._read_backend_line(prompt)
        if line is None:
            return self._take_buffered(len(self._buffer))
        self._buffer += line
        newline_index = self._buffer.find(b"\n")
        end = len(self._buffer) if newline_index < 0 else newline_index + 1
        if size > 0:
            end = min(end, size)
        return self._take_buffered(end)

    def _decode_buffer(self):
        return self._buffer.decode(self.encoding, self.errors)

    def _take_text(self, size):
        text = self._decode_buffer()
        chunk = text[:size]
        byte_len = len(chunk.encode(self.encoding))
        self._buffer = self._buffer[byte_len:]
        return chunk

    def read(self, size=-1):
        self._check_open()
        size = self._normalize_size(size)
        if size == 0:
            return ""
        if size < 0:
            return self._read_bytes(-1).decode(self.encoding, self.errors)

        while len(self._decode_buffer()) < size:
            line = self._read_backend_line()
            if line is None:
                break
            self._buffer += line
        return self._take_text(min(size, len(self._decode_buffer())))

    def _readline(self, size=-1, prompt=""):
        self._check_open()
        size = self._normalize_size(size)
        if size == 0:
            return ""

        prompt_for_read = prompt
        while True:
            text = self._decode_buffer()
            newline_index = text.find("\n")
            if newline_index >= 0:
                self._emit_prompt(prompt_for_read)
                end = newline_index + 1
                if size > 0:
                    end = min(end, size)
                return self._take_text(end)
            if size > 0 and len(text) >= size:
                self._emit_prompt(prompt_for_read)
                return self._take_text(size)

            line = self._read_backend_line(prompt_for_read)
            prompt_for_read = ""
            if line is None:
                return self._take_text(len(self._decode_buffer()))
            self._buffer += line

    def readline(self, size=-1):
        return self._readline(size)

    def _readline_for_input(self, prompt):
        return self._readline(-1, prompt)

    def readlines(self, hint=-1):
        self._check_open()
        hint = self._normalize_size(hint)
        lines = []
        total = 0
        while True:
            line = self.readline()
            if line == "":
                break
            lines.append(line)
            total += len(line)
            if hint > 0 and total >= hint:
                break
        return lines

    def __iter__(self):
        return self

    def __next__(self):
        line = self.readline()
        if line == "":
            raise StopIteration
        return line

    def readable(self):
        return True

    def writable(self):
        return False

    def seekable(self):
        return False

    def isatty(self):
        return False

    def fileno(self):
        return self._fileno

    def close(self):
        if self.closed:
            return
        try:
            if self._closefd and self._fileno != 0:
                os.close(self._fileno)
        finally:
            self.closed = True

    def __enter__(self):
        self._check_open()
        return self

    def __exit__(self, exc_type, exc, traceback):
        self.close()

    def flush(self):
        pass


class McpInputBuffer:
    def __init__(self, text_stream):
        self._text_stream = text_stream

    @property
    def closed(self):
        return self._text_stream.closed

    def _check_open(self):
        self._text_stream._check_open()

    def read(self, size=-1):
        self._check_open()
        return self._text_stream._read_bytes(size)

    def readline(self, size=-1):
        self._check_open()
        return self._text_stream._readline_bytes(size)

    def readlines(self, hint=-1):
        self._check_open()
        hint = self._text_stream._normalize_size(hint)
        lines = []
        total = 0
        while True:
            line = self.readline()
            if line == b"":
                break
            lines.append(line)
            total += len(line)
            if hint > 0 and total >= hint:
                break
        return lines

    def readinto(self, target):
        data = self.read(len(target))
        target[: len(data)] = data
        return len(data)

    def read1(self, size=-1):
        return self.read(size)

    def readinto1(self, target):
        return self.readinto(target)

    def close(self):
        self._text_stream.close()

    def __enter__(self):
        self._check_open()
        return self

    def __exit__(self, exc_type, exc, traceback):
        self.close()

    def readable(self):
        return True

    def writable(self):
        return False

    def seekable(self):
        return False

    def isatty(self):
        return False

    def fileno(self):
        return self._text_stream.fileno()

    def close(self):
        self._text_stream.close()

    def flush(self):
        pass


class McpRawInputBuffer(io.RawIOBase):
    def __init__(self, fileno=0, closefd=False):
        super().__init__()
        self._fileno = fileno
        self._closefd = closefd

    def _check_open(self):
        if self.closed:
            raise ValueError("I/O operation on closed file.")

    def _normalize_size(self, size):
        if size is None:
            return -1
        return operator.index(size)

    def read(self, size=-1):
        self._check_open()
        size = self._normalize_size(size)
        if size == 0:
            return b""
        if size < 0:
            return McpInputStream().buffer.read(size)
        return _mcp_repl.raw_stdin_read(size)

    def readall(self):
        return self.read(-1)

    def readline(self, size=-1):
        self._check_open()
        size = self._normalize_size(size)
        if size == 0:
            return b""

        chunks = []
        remaining = size
        while remaining != 0:
            chunk = self.read(1)
            if chunk == b"":
                break
            chunks.append(chunk)
            if chunk == b"\n":
                break
            if remaining > 0:
                remaining -= 1
        return b"".join(chunks)

    def readlines(self, hint=-1):
        self._check_open()
        hint = self._normalize_size(hint)
        lines = []
        total = 0
        while True:
            line = self.readline()
            if line == b"":
                break
            lines.append(line)
            total += len(line)
            if hint > 0 and total >= hint:
                break
        return lines

    def readinto(self, target):
        data = self.read(len(target))
        target[: len(data)] = data
        return len(data)

    def readable(self):
        return True

    def writable(self):
        return False

    def seekable(self):
        return False

    def isatty(self):
        return False

    def fileno(self):
        return self._fileno

    def close(self):
        if self.closed:
            return
        try:
            if self._closefd and self._fileno != 0:
                os.close(self._fileno)
        finally:
            super().close()

    def flush(self):
        pass


class McpOutputStream:
    encoding = "utf-8"
    errors = "replace"
    closed = False

    def __init__(self, stream):
        self._stream = stream
        self.buffer = McpOutputBuffer(stream)

    def write(self, message):
        if not isinstance(message, str):
            raise TypeError(
                f"write() argument must be str, not {type(message).__name__}"
            )
        self.buffer.write(message.encode(self.encoding, self.errors))
        return len(message)

    def writelines(self, lines):
        for line in lines:
            self.write(line)

    def flush(self):
        pass

    def readable(self):
        return False

    def writable(self):
        return True

    def seekable(self):
        return False

    def isatty(self):
        return False

    def fileno(self):
        return 1 if self._stream == "stdout" else 2

    def close(self):
        pass


class McpOutputBuffer:
    closed = False

    def __init__(self, stream):
        self._stream = stream

    def write(self, data):
        data = bytes(data)
        return _mcp_repl.write_bytes(self._stream, data)

    def flush(self):
        pass

    def readable(self):
        return False

    def writable(self):
        return True

    def seekable(self):
        return False

    def isatty(self):
        return False

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


def _emit_plots(force_figures=None, force_all=False, record_only=False):
    global _plot_known_figures, _plot_hashes, _plot_emitted_this_request
    global _plot_emit_in_progress

    if not record_only and _mcp_repl.take_plot_reset_pending():
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
    try:
        current_fig_num = plt.gcf().number
    except Exception:
        current_fig_num = None
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
            if record_only:
                _plot_hashes[fig_num] = digest
                _plot_emitted_this_request.pop(fig_num, None)
                continue
            emitted_this_request = _plot_emitted_this_request.get(fig_num) == digest
            if emitted_this_request:
                continue
            if _plot_hashes.get(fig_num) == digest and not force_current:
                continue
            _plot_hashes[fig_num] = digest
            _plot_emitted_this_request[fig_num] = digest

        encoded = base64.b64encode(data).decode("ascii")
        is_new = fig_num not in prev_known
        _mcp_repl_flush_original_stdio()
        _mcp_repl.emit_plot_image("image/png", encoded, not bool(is_new), str(fig_num))

    if current_fig_num in new_known:
        try:
            plt.figure(current_fig_num)
        except Exception:
            pass
    with _plot_lock:
        _plot_known_figures = new_known
    _plot_emit_in_progress = False


def _mcp_repl_begin_request():
    global _plot_emitted_this_request
    with _plot_lock:
        _plot_emitted_this_request = {}


def _mcp_repl_emit_plots():
    _emit_plots()


def _mcp_repl_record_background_plots():
    _emit_plots(record_only=True)


def _mcp_repl_flush_original_stdio():
    sys.__stdout__.flush()
    sys.__stderr__.flush()


def _mcp_repl_plot_capable():
    return bool(_plot_capable)


_original_excepthook = sys.excepthook
_original_builtins_open = builtins.open
_original_io_FileIO = io.FileIO
_original_os_fdopen = os.fdopen
_original_os_read = os.read
_original_os_readv = getattr(os, "readv", None)
_mcp_repl_raw_stdin_read_supported = os.name == "posix"
# Keep the original fd 0 identity so duplicated stdin fds still use the bridge.
_mcp_repl_raw_stdin_stat = None
if _mcp_repl_raw_stdin_read_supported:
    try:
        _mcp_repl_raw_stdin_stat = os.fstat(0)
    except OSError:
        pass
_mcp_repl_stdin_path_aliases = frozenset(("/dev/stdin", "/dev/fd/0", "/proc/self/fd/0"))


def _mcp_repl_excepthook(exc_type, exc, traceback):
    if issubclass(exc_type, SystemExit):
        _mcp_repl.request_exit()
        return
    _original_excepthook(exc_type, exc, traceback)


def _mcp_repl_is_raw_stdin_fd(fd):
    if not _mcp_repl_raw_stdin_read_supported:
        return False
    if _mcp_repl_raw_stdin_stat is None:
        return fd == 0
    try:
        stat = os.fstat(fd)
    except OSError:
        return False
    return (
        stat.st_dev == _mcp_repl_raw_stdin_stat.st_dev
        and stat.st_ino == _mcp_repl_raw_stdin_stat.st_ino
    )


def _mcp_repl_is_raw_stdin_path(file):
    try:
        path = os.fspath(file)
    except TypeError:
        return False
    if isinstance(path, bytes):
        path = os.fsdecode(path)
    return os.path.normpath(path) in _mcp_repl_stdin_path_aliases


def _mcp_repl_stdin_read_mode(mode):
    return isinstance(mode, str) and mode in ("r", "rt", "tr", "rb", "br")


def _mcp_repl_unbuffered_binary_stdin_mode(mode, buffering):
    return (
        isinstance(mode, str)
        and "b" in mode
        and _mcp_repl_stdin_read_mode(mode)
        and operator.index(buffering) == 0
    )


def _mcp_repl_os_fdopen_closefd(args, kwargs):
    # os.fdopen forwards positional args to open(fd, mode, ...), where
    # closefd is the fifth argument after mode. Respecting that slot preserves
    # callers that intentionally keep a duplicated stdin fd usable after the
    # bridge returns its own stdin wrapper.
    if len(args) >= 5:
        return args[4]
    return kwargs.get("closefd", True)


def _mcp_repl_os_fdopen_buffering(args, kwargs):
    if len(args) >= 1:
        return args[0]
    return kwargs.get("buffering", -1)


def _mcp_repl_stdin_stream_for_mode(mode, fileno=0, closefd=False):
    stream = McpInputStream(fileno, closefd)
    if "b" in mode:
        return stream.buffer
    return stream


def _mcp_repl_open(
    file,
    mode="r",
    buffering=-1,
    encoding=None,
    errors=None,
    newline=None,
    closefd=True,
    opener=None,
):
    try:
        fd = operator.index(file)
    except TypeError:
        if (
            opener is None
            and closefd
            and _mcp_repl_is_raw_stdin_path(file)
            and _mcp_repl_stdin_read_mode(mode)
        ):
            if _mcp_repl_unbuffered_binary_stdin_mode(mode, buffering):
                return McpRawInputBuffer()
            return _mcp_repl_stdin_stream_for_mode(mode)
        return _original_builtins_open(
            file, mode, buffering, encoding, errors, newline, closefd, opener
        )
    if (
        opener is None
        and _mcp_repl_is_raw_stdin_fd(fd)
        and _mcp_repl_stdin_read_mode(mode)
    ):
        if _mcp_repl_unbuffered_binary_stdin_mode(mode, buffering):
            return McpRawInputBuffer(fd, closefd)
        return _mcp_repl_stdin_stream_for_mode(mode, fd, closefd)
    return _original_builtins_open(
        file, mode, buffering, encoding, errors, newline, closefd, opener
    )


def _mcp_repl_os_fdopen(fd, mode="r", *args, **kwargs):
    fd = operator.index(fd)
    buffering = _mcp_repl_os_fdopen_buffering(args, kwargs)
    closefd = _mcp_repl_os_fdopen_closefd(args, kwargs)
    if _mcp_repl_is_raw_stdin_fd(fd) and _mcp_repl_stdin_read_mode(mode):
        if _mcp_repl_unbuffered_binary_stdin_mode(mode, buffering):
            return McpRawInputBuffer(fd, closefd)
        return _mcp_repl_stdin_stream_for_mode(mode, fd, closefd)
    return _original_os_fdopen(fd, mode, *args, **kwargs)


class _McpReplFileIOMeta(type):
    def __instancecheck__(cls, instance):
        return isinstance(instance, (_original_io_FileIO, McpRawInputBuffer))

    def __subclasscheck__(cls, subclass):
        return issubclass(subclass, (_original_io_FileIO, McpRawInputBuffer))


class _McpReplFileIO(_original_io_FileIO, metaclass=_McpReplFileIOMeta):
    def __new__(cls, file, mode="r", closefd=True, opener=None):
        try:
            fd = operator.index(file)
        except TypeError:
            if (
                opener is None
                and closefd
                and _mcp_repl_is_raw_stdin_path(file)
                and _mcp_repl_stdin_read_mode(mode)
            ):
                return McpRawInputBuffer()
            return super().__new__(cls)
        if (
            opener is None
            and _mcp_repl_is_raw_stdin_fd(fd)
            and _mcp_repl_stdin_read_mode(mode)
        ):
            return McpRawInputBuffer(fd, closefd)
        return super().__new__(cls)

    def __init__(self, file, mode="r", closefd=True, opener=None):
        super().__init__(file, mode, closefd=closefd, opener=opener)


def _mcp_repl_fill_readv_buffers(buffers, data):
    offset = 0
    for view in buffers:
        if offset >= len(data):
            break
        count = min(view.nbytes, len(data) - offset)
        view[:count] = data[offset : offset + count]
        offset += count
    return offset


def _mcp_repl_os_read(fd, n):
    fd = operator.index(fd)
    if _mcp_repl_is_raw_stdin_fd(fd):
        n = operator.index(n)
        if n > sys.maxsize or n < -sys.maxsize - 1:
            raise OverflowError("Python int too large to convert to C ssize_t")
        if n < 0:
            raise OSError(errno.EINVAL, os.strerror(errno.EINVAL))
        return _mcp_repl.raw_stdin_read(n)
    return _original_os_read(fd, n)


def _mcp_repl_os_readv(fd, buffers):
    fd = operator.index(fd)
    if not _mcp_repl_is_raw_stdin_fd(fd):
        return _original_os_readv(fd, buffers)
    views = []
    total = 0
    for buffer in buffers:
        view = memoryview(buffer)
        if view.readonly:
            raise TypeError("readv buffers must be writable")
        view = view.cast("B")
        views.append(view)
        total += view.nbytes
    if total > sys.maxsize:
        raise OverflowError("Python int too large to convert to C ssize_t")
    if total == 0:
        return 0
    return _mcp_repl_fill_readv_buffers(views, _mcp_repl.raw_stdin_read(total))


builtins.input = _input
# The worker keeps a real fd 0 so Unix readiness checks and fork+exec children
# behave like a normal REPL. Python-level integer-fd reads of that same stdin
# and path aliases to it must still go through sideband so the server can
# account for consumed input.
builtins.open = _mcp_repl_open
io.open = _mcp_repl_open
io.FileIO = _McpReplFileIO
_io.open = _mcp_repl_open
_io.FileIO = _McpReplFileIO
pydoc.pager = _pydoc_plainpager
os.fdopen = _mcp_repl_os_fdopen
os.read = _mcp_repl_os_read
if _original_os_readv is not None:
    os.readv = _mcp_repl_os_readv
if _mcp_repl_posix is not None:
    _mcp_repl_posix.read = _mcp_repl_os_read
    if _original_os_readv is not None:
        _mcp_repl_posix.readv = _mcp_repl_os_readv
sys.excepthook = _mcp_repl_excepthook
_mcp_repl.set_python_prompts(_mcp_repl_ps1, _mcp_repl_ps2)
sys.ps1 = _mcp_repl_suppressed_ps1
sys.ps2 = _mcp_repl_suppressed_ps2
_mcp_repl_stdin = McpInputStream()
sys.stdin = _mcp_repl_stdin
# In vanilla Python, sys.__stdin__ preserves the startup stdin object. In this
# embedded REPL the startup stdin is mcp-repl-managed, and leaving CPython's
# original fd-backed object exposed would let user code bypass sideband input
# accounting and keep requests busy after consuming bytes.
sys.__stdin__ = _mcp_repl_stdin
sys.stdout = McpOutputStream("stdout")
sys.stderr = McpOutputStream("stderr")
