"""Ordinary console output, images, retention, and preview scenarios."""

import base64
import os
import time
from pathlib import Path

from .control import publish_marker
from .io import (
    emit_large_output,
    write_all,
)
from .protocol import (
    ConsoleKind,
    send,
    send_batch,
    send_output,
    wait_for_server_to_process_sideband,
)
from .state import WorkerContext

PENDING_TEXT_BUDGET = 8 * 1024 * 1024
PNG_1X1 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42Y"
    "AAAAASUVORK5CYII="
)


def overflow_cell_retention_limit(context: WorkerContext, source: str) -> None:
    release = context.root / "zod-release-retention-output"
    completed = context.root / "zod-retention-completed"
    os.mkfifo(completed)
    os.mkfifo(release)
    with release.open("rb", buffering=0) as checkpoint:
        assert checkpoint.read(1) == b"1"
    for _ in range(128):
        send_output(context.writer, "x" * PENDING_TEXT_BUDGET)
    send_output(context.writer, "tail\n")
    send(context.writer, {"kind": "completed"})
    wait_for_server_to_process_sideband(context.reader, context.writer)
    with completed.open("wb", buffering=0) as checkpoint:
        assert checkpoint.write(b"1") == 1


def overflow_cell_output_file(context: WorkerContext, source: str) -> None:
    release = context.root / "zod-release-spooled-output"
    processed = context.root / "zod-spooled-output-processed"
    os.mkfifo(processed)
    os.mkfifo(release)
    for character, length in (
        ("x", 3 * PENDING_TEXT_BUDGET + 7),
        ("y", PENDING_TEXT_BUDGET + 7),
    ):
        with release.open("rb", buffering=0) as checkpoint:
            assert checkpoint.read(1) == b"1"
        send_output(context.writer, character * length)
        if character == "y":
            send(context.writer, {"kind": "completed"})
        wait_for_server_to_process_sideband(context.reader, context.writer)
        with processed.open("wb", buffering=0) as checkpoint:
            assert checkpoint.write(b"1") == 1


def overflow_console_output(context: WorkerContext, source: str) -> None:
    send_output(context.writer, "x" * (PENDING_TEXT_BUDGET + 7))
    send(context.writer, {"kind": "completed"})


def preview_image_limit(context: WorkerContext, source: str) -> None:
    send_output(context.writer, "before oversized image\n")
    data = base64.b64encode(b"\0" * (6 * 1024 * 1024 + 3)).decode("ascii")
    send(context.writer, {"kind": "image", "data": data, "mime_type": "image/png"})
    send_output(context.writer, "before accepted image\n")
    send(context.writer, {"kind": "image", "data": PNG_1X1, "mime_type": "image/png"})
    send_output(context.writer, "after accepted image\n")
    send(context.writer, {"kind": "completed"})


def preview_allocation_image(context: WorkerContext, source: str) -> None:
    if source.endswith("then fail"):
        checkpoint = Path(os.environ["ZOD_STARTUP_CONTROL"]).with_name("image-release")
        with checkpoint.open("rb", buffering=0) as release:
            assert release.read(1) == b"1"
    send(
        context.writer,
        {
            "kind": "image",
            "data": "A" * (8 * 1024 * 1024),
            "mime_type": "image/png",
        },
    )
    if source.endswith("and text"):
        send_output(context.writer, "preview head\n" + "x" * 32768 + "\npreview tail\n")
    if source.endswith("then fail"):
        wait_for_server_to_process_sideband(context.reader, context.writer)
        Path(os.environ["ZOD_STARTUP_CONTROL"]).write_text("block")
        os._exit(86)
    send(context.writer, {"kind": "completed"})


def overflow_image_retention(context: WorkerContext, source: str) -> None:
    data = base64.b64encode(b"\0" * (6 * 1024 * 1024 + 3)).decode("ascii")
    for _ in range(10):
        send(context.writer, {"kind": "image", "data": data, "mime_type": "image/png"})
    data = base64.b64encode(b"\0" * (4 * 1024 * 1024)).decode("ascii")
    send(context.writer, {"kind": "image", "data": data, "mime_type": "image/png"})
    send(context.writer, {"kind": "completed"})


def emit_largest_retained_image(context: WorkerContext, source: str) -> None:
    data = base64.b64encode(b"\0" * (64 * 1024 * 1024)).decode("ascii")
    send(context.writer, {"kind": "image", "data": data, "mime_type": "image/png"})
    send(context.writer, {"kind": "completed"})


def preview_rejected_image(context: WorkerContext, source: str) -> None:
    send(context.writer, {"kind": "image", "data": "AAAA", "mime_type": "m" * 65537})
    send(context.writer, {"kind": "completed"})


def preview_recovery_intervals(context: WorkerContext, source: str) -> None:
    directory = Path(os.environ["MCP_CONSOLE_TEST_PREVIEW_DIRECTORY"])
    release = directory / "preview-release"
    processed = directory / "preview-processed"
    for index in range(3):
        with release.open("rb", buffering=0) as checkpoint:
            assert checkpoint.read(1) == b"1"
        send_output(
            context.writer,
            f"interval {index} head\n" + "x" * 32768 + f"\ninterval {index} tail\n",
        )
        wait_for_server_to_process_sideband(context.reader, context.writer)
        with processed.open("wb", buffering=0) as checkpoint:
            assert checkpoint.write(b"1") == 1
    with release.open("rb", buffering=0) as checkpoint:
        assert checkpoint.read(1) == b"1"
    send(context.writer, {"kind": "completed"})


def preview_recovery_cell(context: WorkerContext, source: str) -> None:
    number = int(source.removeprefix("preview recovery cell "))
    send_output(
        context.writer,
        f"cell {number} head\n" + "x" * 32768 + f"\ncell {number} tail\n",
    )
    send(context.writer, {"kind": "completed"})


def preview_alternating_bytes(context: WorkerContext, source: str) -> None:
    for index in range(8192):
        send_output(
            context.writer,
            "b" if index % 2 else "a",
            ConsoleKind.DIAGNOSTIC if index % 2 else ConsoleKind.OUTPUT,
        )
    send(context.writer, {"kind": "completed"})


def preview_producer_suffix(context: WorkerContext, source: str) -> None:
    send_output(context.writer, "preview head\n")
    unit, count = (
        ("ab", 100000) if source == "preview same producer" else ("a€🙂b", 10000)
    )
    for _ in range(count):
        send_output(context.writer, unit)
    if source == "preview unicode suffix":
        send_output(context.writer, "\b🙂\b\r\n")
    elif source == "preview unicode replacement":
        send_output(context.writer, "\b" * 8192 + "\r\bfinal 🙂\bframe\r\n")
    else:
        send_output(context.writer, "\n")
    send_output(context.writer, "preview tail: final diagnostic\n")
    send(context.writer, {"kind": "completed"})


def preview_unicode_lines(context: WorkerContext, source: str) -> None:
    line = "line €🙂 " + "x" * 48 + "\r\n"
    send_output(context.writer, "preview head\n" + line * 512 + "preview tail 🙂\r\n")
    send(context.writer, {"kind": "completed"})


def preview_large_output(context: WorkerContext, source: str) -> None:
    send_output(context.writer, "preview head\n")
    if source == "preview huge line":
        send_output(context.writer, "x" * (2 * PENDING_TEXT_BUDGET))
    elif source in {"preview tiny events", "preview many tiny events"}:
        for index in range(100000 if source == "preview many tiny events" else 12000):
            send_output(
                context.writer,
                "ab",
                ConsoleKind.DIAGNOSTIC if index % 2 else ConsoleKind.OUTPUT,
            )
    else:
        for _ in range(5000):
            send_output(context.writer, "\r" + "x" * 2048)
        send_output(context.writer, "\rprogress final")
    send_output(context.writer, "\npreview tail: final diagnostic\n")
    send(context.writer, {"kind": "image", "data": PNG_1X1, "mime_type": "image/png"})
    send_output(context.writer, "after final image\n")
    send(context.writer, {"kind": "completed"})


def emit_stdout(context: WorkerContext, source: str) -> None:
    # Native stdout must follow observed cell entry before the test can expect
    # its entire payload in that cell's log. The streams have no shared order.
    wait_for_server_to_process_sideband(context.reader, context.writer)
    release = context.root / "zod-release-stdout-completion"
    os.mkfifo(release)
    write_all(1, b"zod stdout ")
    for part in ("👩", "🏽", "\u200d", "💻", "\n"):
        write_all(1, part.encode())
    emit_large_output(1, b"")
    with release.open("rb", buffering=0) as checkpoint:
        assert checkpoint.read(1) == b"1"
    send(context.writer, {"kind": "completed"})


def preview_ansi_redraw(context: WorkerContext, source: str) -> None:
    send_output(context.writer, "preview head\n" + "x" * 32768)
    send_output(context.writer, "\x1b[2K")
    send_output(context.writer, "\x1b[" + "0;" * 100000 + "2K")
    send_output(context.writer, "\x1b]52;c;" + "hidden\n" * 100000 + "\x1b\\")
    for _ in range(5000):
        send_output(context.writer, "\r\x1b[2K\x1b[32m" + "x" * 2048 + "\x1b[0m")
    send_output(
        context.writer,
        "\r\x1b[K\x1b[32mprogress final\x1b[0m\npreview tail: final diagnostic\n"
        "\x1b]unterminated before image",
    )
    send(context.writer, {"kind": "image", "data": PNG_1X1, "mime_type": "image/png"})
    send_output(context.writer, "after final image\n")
    send(context.writer, {"kind": "completed"})


def redraw_across_polls(context: WorkerContext, source: str) -> None:
    release = context.root / "zod-release-redraw"
    completion = context.root / "zod-release-redraw-completion"
    os.mkfifo(release)
    os.mkfifo(completion)
    # Separate phases cannot reconnect to the previous phase's still-open
    # writer and receive EOF when that writer closes.
    # The first response must settle before this segment can be emitted.
    with release.open("rb", buffering=0) as checkpoint:
        assert checkpoint.read(1) == b"1"
    send_output(context.writer, "output 10%\r")
    wait_for_server_to_process_sideband(context.reader, context.writer)
    publish_marker(context.root / "zod-redraw-ready")
    with completion.open("rb", buffering=0) as checkpoint:
        assert checkpoint.read(1) == b"1"

    send_output(context.writer, "output 100%\n")
    send(context.writer, {"kind": "completed"})


def stress_redraws(context: WorkerContext, source: str) -> None:
    payload = "x" * 2048
    send_batch(
        context.writer,
        [
            {
                "kind": ConsoleKind.OUTPUT,
                "data": f"\rstress {index}: {payload}",
            }
            for index in range(100)
        ],
    )
    send_output(context.writer, "\rstress final\nuseful output\n")
    send(context.writer, {"kind": "completed"})


def language_error(context: WorkerContext, source: str) -> None:
    send_output(
        context.writer,
        "zod language error\n",
        ConsoleKind.DIAGNOSTIC,
    )
    send(context.writer, {"kind": "completed"})


def complete_silently(context: WorkerContext, source: str) -> None:
    send(context.writer, {"kind": "completed"})


def emit_console_kinds(context: WorkerContext, source: str) -> None:
    send_output(context.writer, "zod output\n")
    send_output(context.writer, "zod diagnostic\n", ConsoleKind.DIAGNOSTIC)
    send(context.writer, {"kind": "completed"})


def emit_image(context: WorkerContext, source: str) -> None:
    send_output(context.writer, "before image\n")
    send(
        context.writer,
        {
            "kind": "image",
            "data": PNG_1X1,
            "mime_type": "image/png",
        },
    )
    send_output(context.writer, "after image\n")
    send(context.writer, {"kind": "completed"})


def emit_image_before_completion(context: WorkerContext, source: str) -> None:
    publish_marker(context.root / "zod-image-evaluation-started")
    while not (context.root / "zod-release-image").exists():
        time.sleep(0.01)
    if source == "emit output and image before completion":
        send_output(context.writer, "before pending image\n")
    send(
        context.writer,
        {
            "kind": "image",
            "data": PNG_1X1,
            "mime_type": "image/png",
        },
    )
    if source == "emit output and image before completion":
        send_output(context.writer, "after pending image\n")
    wait_for_server_to_process_sideband(context.reader, context.writer)
    publish_marker(context.root / "zod-image-processed")
    while not (context.root / "zod-release-image-completion").exists():
        time.sleep(0.01)
    send(context.writer, {"kind": "completed"})


def echo(context: WorkerContext, source: str) -> None:
    payload = source.removeprefix("echo ")
    for output in ("zod: ", f"{payload}\n"):
        send_output(context.writer, output)
    send(context.writer, {"kind": "completed"})
    if context.idle_input_received:
        wait_for_server_to_process_sideband(context.reader, context.writer)
        publish_marker(context.root / "zod-idle-input-received")
        context.idle_input_received = False


def recording_intervals(context: WorkerContext, source: str) -> None:
    for text in ("before image\n", "after image\n"):
        send_output(context.writer, text)
        send(
            context.writer, {"kind": "image", "data": PNG_1X1, "mime_type": "image/png"}
        )
    send(context.writer, {"kind": "completed"})
    wait_for_server_to_process_sideband(context.reader, context.writer)
    # The client releases post output only after receiving the completed cell.
    # Its response cut must not race the idle interval's publications.
    release = context.root / "zod-recording-post-release"
    os.mkfifo(release)
    with release.open("rb", buffering=0) as gate:
        assert gate.read(1) == b"1"
    send_output(context.writer, "between cells\n")
    send(context.writer, {"kind": "image", "data": PNG_1X1, "mime_type": "image/png"})
    wait_for_server_to_process_sideband(context.reader, context.writer)
    publish_marker(context.root / "zod-recording-post-observed")
