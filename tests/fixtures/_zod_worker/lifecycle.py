"""Interrupt, completion, restart state, shutdown, and process-exit scenarios."""

import os
import select
import signal
import time
from pathlib import Path

from .control import (
    emit_test_event,
    publish_marker,
    wait_for_test_control,
)
from .io import emit_large_output
from .output import PNG_1X1, echo
from .protocol import (
    close_sideband,
    send,
    send_output,
    wait_for_server_to_process_sideband,
)
from .state import LoopAction, WorkerContext


def wait_for_interrupt(context: WorkerContext, source: str) -> None:
    target = int(source.removeprefix("wait for interrupt: "))
    previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT})
    try:
        emit_test_event(context, target, "worker_operation_started")
        received = signal.sigwait({signal.SIGINT})
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
    assert received == signal.SIGINT, received
    context.received_sigint = False
    emit_test_event(context, target, "worker_interrupt_observed", signal=received)
    send_output(context.writer, "zod interrupted\n")
    emit_test_event(context, target, "worker_operation_completed")
    send(context.writer, {"kind": "completed"})


def interrupt(context: WorkerContext, source: str) -> None:
    publish_marker(context.root / "zod-interrupt-evaluation-started")
    while not context.received_sigint:
        time.sleep(0.01)
    context.received_sigint = False
    if release := os.environ.get("MCP_CONSOLE_TEST_INTERRUPT_RELEASE"):
        with Path(release).open("rb", buffering=0) as checkpoint:
            assert checkpoint.read(1) == b"1"
    send_output(context.writer, "zod interrupted\n")
    send(context.writer, {"kind": "completed"})


def stall(context: WorkerContext, source: str) -> None:
    publish_marker(context.root / "zod-stalled")
    while True:
        signal.pause()


def stall_operation(context: WorkerContext, source: str) -> None:
    operation = int(source.removeprefix("stall: "))
    emit_test_event(context, operation, "worker_operation_started")
    emit_test_event(
        context,
        operation,
        "parent_operation_stalled",
        pid=os.getpid(),
        process_group=os.getpgrp(),
    )
    while True:
        signal.pause()


def close_sideband_with_unread_shutdown(
    context: WorkerContext, source: str
) -> LoopAction:
    send_output(context.writer, "zod waiting for shutdown\n")
    while os.read(0, 8192) != b"":
        pass
    expected = b'{"kind":"shutdown"}\n'
    # Leave the shutdown newline unread when both pipe directions close.
    queued = bytearray()
    while len(queued) < len(expected) - 1:
        chunk = os.read(context.reader.fileno(), len(expected) - 1 - len(queued))
        assert chunk, queued
        queued.extend(chunk)
    assert queued == expected[:-1], queued
    assert select.select([context.reader], [], [], 10)[0]
    close_sideband(context.reader, context.writer)
    return LoopAction.STOP


def exit_unexpectedly(context: WorkerContext, source: str) -> None:
    if source == "exit after output":
        send_output(context.writer, "zod output before exit\n")
    elif source.startswith("exit after output: "):
        send_output(context.writer, source.removeprefix("exit after output: "))
    os._exit(86)


def exit_zero(context: WorkerContext, source: str) -> None:
    os._exit(0)


def wait_for_stdin_close(context: WorkerContext, source: str) -> None:
    publish_marker(context.root / "zod-waiting-for-stdin-close")
    assert os.read(0, 1) == b"", "Zod received input instead of stdin EOF"
    # Stdin closure and cell-log retirement travel through independent
    # transports. Emit only after both, so this is all unretained output.
    with Path(os.environ["ZOD_STDIN_CLOSE_RELEASE"]).open("rb") as release:
        assert release.read(1) == b"1"
    emit_large_output(1, b"zod stdin closed\n")
    send(context.writer, {"kind": "completed"})


def set_controlled_restart_state(context: WorkerContext, source: str) -> None:
    context.controlled_restart_state = "old"
    publish_marker(
        context.root / "zod-controlled-restart-old-worker",
        str(os.getpid()),
    )
    send_output(context.writer, "zod controlled state: old\n")
    send(context.writer, {"kind": "completed"})


def inspect_controlled_restart_state(context: WorkerContext, source: str) -> None:
    context.controlled_restart_evaluations += 1
    with (context.root / "zod-controlled-restart-cell-evaluations").open(
        "a",
        encoding="utf-8",
    ) as evaluations:
        evaluations.write(
            f"{os.getpid()} {context.controlled_restart_state} "
            f"{context.controlled_restart_evaluations}\n"
        )
    send_output(
        context.writer,
        f"zod controlled state: {context.controlled_restart_state}; "
        f"evaluation={context.controlled_restart_evaluations}\n",
    )
    send(context.writer, {"kind": "completed"})


def shutdown_output_checkpoints(context: WorkerContext, source: str) -> None:
    emit_test_event(context, 0, "evaluation_started")
    wait_for_test_control(context, 0, "emit_output")
    send_output(context.writer, "before shutdown\n")
    send(context.writer, {"kind": "image", "data": PNG_1X1, "mime_type": "image/png"})
    send_output(context.writer, "after image\n")
    wait_for_server_to_process_sideband(context.reader, context.writer)
    emit_test_event(context, 0, "output_processed")
    ownership = wait_for_test_control(context, 0, "observe_poll_ownership")
    request = ownership["request"]
    assert isinstance(request, int), ownership
    # Poll stdin reaches Zod only after the server claims the evaluation wait.
    assert os.read(0, 1) == b"p"
    emit_test_event(context, request, "poll_ownership_observed", target_operation=0)
    wait_for_test_control(context, 0, "complete")
    send(context.writer, {"kind": "completed"})
    wait_for_server_to_process_sideband(context.reader, context.writer)
    emit_test_event(context, 0, "completion_processed")


def complete_before_restart_checkpoint(context: WorkerContext, source: str) -> None:
    publish_marker(context.root / "zod-completion-started")
    while not (context.root / "zod-release-completion").exists():
        time.sleep(0.01)
    send(context.writer, {"kind": "completed"})
    wait_for_server_to_process_sideband(context.reader, context.writer)
    publish_marker(context.root / "zod-completion-processed")


def report_process_group(context: WorkerContext, source: str) -> None:
    send_output(context.writer, f"zod process group: {os.getpgrp()}\n")
    send(context.writer, {"kind": "completed"})


def complete_after_release(context: WorkerContext, source: str) -> None:
    publish_marker(context.root / "zod-evaluation-started")
    while not (context.root / "zod-release-evaluation").exists():
        time.sleep(0.01)
    echo(context, source)


def hold_after_output(context: WorkerContext, source: str) -> None:
    send_output(context.writer, source.removeprefix("hold after output: "))
    wait_for_server_to_process_sideband(context.reader, context.writer)
    publish_marker(context.root / "zod-output-held")
    while not (context.root / "zod-release-evaluation").exists():
        time.sleep(0.01)
    send(context.writer, {"kind": "completed"})


def output_then_complete_after_release(context: WorkerContext, source: str) -> None:
    release = context.root / "zod-release-cell-output"
    os.mkfifo(release)
    publish_marker(context.root / "zod-cell-output-pending")
    with release.open("rb", buffering=0) as checkpoint:
        assert checkpoint.read(1) == b"1"
    send_output(context.writer, "zod cell output before completion\n")
    wait_for_server_to_process_sideband(context.reader, context.writer)
    publish_marker(context.root / "zod-cell-output-processed")
    while not (context.root / "zod-release-evaluation").exists():
        time.sleep(0.01)
    echo(context, source)
