import importlib.util
import io
import os
import queue
import tempfile
import threading
import unittest
from pathlib import Path


def load_module():
    script_path = Path(__file__).with_name("codex-stdio-trace-win.py")
    spec = importlib.util.spec_from_file_location("codex_stdio_trace_win", script_path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class ForwardStreamTests(unittest.TestCase):
    def setUp(self):
        self.module = load_module()
        self.temp_dir = tempfile.TemporaryDirectory()
        raw_path = Path(self.temp_dir.name) / "wire.jsonl"
        pretty_path = Path(self.temp_dir.name) / "wire.pretty.json"
        self.log = self.module.Logger(raw_path, pretty_path)
        self._closers = []

    def tearDown(self):
        self.close_all(self.log.raw_file.close, self.log.pretty_file.close)
        for closer in reversed(self._closers):
            try:
                closer()
            except OSError:
                pass
        self.temp_dir.cleanup()

    def close_all(self, *closers):
        for closer in closers:
            try:
                closer()
            except OSError:
                pass

    def test_stdin_forwarding_does_not_wait_for_eof(self):
        src_read_fd, src_write_fd = os.pipe()
        dst_read_fd, dst_write_fd = os.pipe()
        self._closers.extend([lambda: os.close(dst_read_fd), lambda: os.close(src_write_fd)])

        src = io.BufferedReader(os.fdopen(src_read_fd, "rb", buffering=0))
        dst = os.fdopen(dst_write_fd, "wb", buffering=0)
        self._closers.extend([src.close, dst.close])

        forward_thread = threading.Thread(
            target=self.module.forward_stream,
            args=(self.log, "stdin", src, dst, False),
            daemon=True,
        )
        forward_thread.start()

        output_queue: queue.Queue[bytes] = queue.Queue()

        def read_forwarded():
            output_queue.put(os.read(dst_read_fd, 4096))

        read_thread = threading.Thread(target=read_forwarded, daemon=True)
        read_thread.start()

        payload = b'{"jsonrpc":"2.0","method":"initialize"}\n'
        os.write(src_write_fd, payload)

        try:
            forwarded = output_queue.get(timeout=0.5)
        except queue.Empty as exc:
            self.close_all(lambda: os.close(src_write_fd))
            forward_thread.join(timeout=2.0)
            self.close_all(src.close, dst.close, lambda: os.close(dst_read_fd))
            self.fail("forward_stream did not forward the short stdin payload before EOF")

        self.assertEqual(forwarded, payload)

        os.close(src_write_fd)
        self._closers.pop()
        forward_thread.join(timeout=2.0)
        self.assertFalse(forward_thread.is_alive(), "forward_stream did not exit after stdin EOF")


if __name__ == "__main__":
    unittest.main()
