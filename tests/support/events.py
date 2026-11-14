"""Blocking file-change and process-exit checkpoints for public boundary tests."""

import ctypes
import os
import select
import socket
import sys
from concurrent.futures import CancelledError
from pathlib import Path


class Events:
    def __init__(self) -> None:
        self.files: dict[Path, int] = {}
        self.processes: dict[int, int] = {}
        self.readable: list[int] = []
        self.native = sys.platform in {"darwin", "linux"}
        self.cancelled = False
        self.closed = False
        if sys.platform == "darwin":
            self.queue = select.kqueue()
        elif sys.platform == "linux":
            self.libc = ctypes.CDLL(None, use_errno=True)
            self.libc.inotify_add_watch.argtypes = [
                ctypes.c_int,
                ctypes.c_char_p,
                ctypes.c_uint32,
            ]
            self.fd = self.libc.inotify_init1(os.O_CLOEXEC | os.O_NONBLOCK)
            if self.fd < 0:
                raise OSError(ctypes.get_errno(), "inotify_init1 failed")
        self.cancel_reader, self.cancel_writer = socket.socketpair()
        if sys.platform == "darwin":
            self.queue.control(
                [
                    select.kevent(
                        self.cancel_reader.fileno(),
                        filter=select.KQ_FILTER_READ,
                        flags=select.KQ_EV_ADD,
                    )
                ],
                0,
                0,
            )

    def cancel(self) -> None:
        """Wake a blocked wait; cancellation remains set for subsequent waits."""
        assert not self.closed
        if not self.cancelled:
            self.cancelled = True
            self.cancel_writer.sendall(b"1")

    def watch_file(self, path: Path) -> None:
        if path in self.files:
            return
        if not self.native:
            self.files[path] = 0
            return
        if sys.platform == "darwin":
            descriptor = os.open(path, os.O_EVTONLY | os.O_CLOEXEC)
            self.files[path] = descriptor

            self.queue.control(
                [
                    select.kevent(
                        descriptor,
                        filter=select.KQ_FILTER_VNODE,
                        flags=select.KQ_EV_ADD | select.KQ_EV_CLEAR,
                        fflags=select.KQ_NOTE_WRITE
                        | select.KQ_NOTE_RENAME
                        | select.KQ_NOTE_DELETE,
                    )
                ],
                0,
                0,
            )
        else:
            # MODIFY, ATTRIB, MOVED_FROM/TO, CREATE, DELETE, DELETE_SELF, MOVE_SELF.
            descriptor = self.libc.inotify_add_watch(self.fd, os.fsencode(path), 0xFC6)
            if descriptor < 0:
                raise OSError(ctypes.get_errno(), f"cannot watch {path}")
            self.files[path] = descriptor

    def watch_tree(self, root: Path, *, recursive: bool) -> None:
        """Subscribe before discovery, including newly created directories."""
        # A path created while its parent was being subscribed reported no
        # change, so repeat discovery until a pass finds nothing new to watch.
        while True:
            watched = len(self.files)
            ancestor = root
            while not ancestor.exists():
                ancestor = ancestor.parent
            self.watch_file(ancestor)
            if recursive and root.exists():
                for directory in root.rglob("*"):
                    if directory.is_dir():
                        try:
                            self.watch_file(directory)
                        except FileNotFoundError:
                            # A worker may retire a temporary directory during discovery.
                            continue
            if len(self.files) == watched:
                return

    def watch_readable(self, descriptor: int) -> None:
        """Report readable input as a change until the caller consumes it."""
        if sys.platform == "darwin":
            self.queue.control(
                [
                    select.kevent(
                        descriptor,
                        filter=select.KQ_FILTER_READ,
                        flags=select.KQ_EV_ADD,
                    )
                ],
                0,
                0,
            )
        self.readable.append(descriptor)

    def watch_process(self, pid: int) -> None:
        assert self.native
        assert pid not in self.processes
        if sys.platform == "darwin":
            self.queue.control(
                [
                    select.kevent(
                        pid,
                        filter=select.KQ_FILTER_PROC,
                        flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT,
                        fflags=select.KQ_NOTE_EXIT,
                    )
                ],
                0,
                0,
            )
            self.processes[pid] = pid
        else:
            self.processes[pid] = os.pidfd_open(pid)

    def wait(self, timeout: float) -> set[int]:
        """Return exited PIDs, with -1 indicating a filesystem change."""
        if self.cancelled:
            raise CancelledError("checkpoint wait cancelled")
        if not self.native:
            # Windows marker waits remain portable. Socket readiness supplies
            # cancellation; the bounded interval only schedules path discovery.
            if select.select([self.cancel_reader], [], [], min(timeout, 0.01))[0]:
                raise CancelledError("checkpoint wait cancelled")
            return {-1}
        if sys.platform == "darwin":
            events = self.queue.control(
                None,
                1 + len(self.files) + len(self.processes) + len(self.readable),
                timeout,
            )
            if any(
                event.filter == select.KQ_FILTER_READ
                and event.ident == self.cancel_reader.fileno()
                for event in events
            ):
                raise CancelledError("checkpoint wait cancelled")
            return {
                event.ident if event.filter == select.KQ_FILTER_PROC else -1
                for event in events
            }
        ready, _, _ = select.select(
            [self.cancel_reader, self.fd, *self.readable, *self.processes.values()],
            [],
            [],
            timeout,
        )
        if self.cancel_reader in ready:
            raise CancelledError("checkpoint wait cancelled")
        result = {
            pid for pid, descriptor in self.processes.items() if descriptor in ready
        }
        for pid in result:
            os.close(self.processes.pop(pid))
        if self.fd in ready:
            os.read(self.fd, 64 * 1024)
            result.add(-1)
        if not set(self.readable).isdisjoint(ready):
            result.add(-1)
        return result

    def close(self) -> None:
        if self.closed:
            return
        self.closed = True
        self.cancel_reader.close()
        self.cancel_writer.close()
        if sys.platform == "darwin":
            self.queue.close()
            for descriptor in self.files.values():
                os.close(descriptor)
        elif sys.platform == "linux":
            os.close(self.fd)
            for descriptor in self.processes.values():
                os.close(descriptor)

    def __enter__(self) -> "Events":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()
