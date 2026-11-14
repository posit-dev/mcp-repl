"""Literal command lookup followed by the ordered parameterized command path."""

from collections.abc import Callable

from . import failures, input, lifecycle, output, preparation, probes
from .state import LoopAction, WorkerContext

EXACT_HANDLERS: dict[str, Callable[[WorkerContext, str], LoopAction | None]] = {
    # Output
    "overflow cell retention limit": output.overflow_cell_retention_limit,
    "overflow cell output file": output.overflow_cell_output_file,
    "overflow console output": output.overflow_console_output,
    "preview image limit": output.preview_image_limit,
    "overflow image retention": output.overflow_image_retention,
    "emit largest retained image": output.emit_largest_retained_image,
    "preview allocation image": output.preview_allocation_image,
    "preview allocation image then fail": output.preview_allocation_image,
    "preview allocation image and text": output.preview_allocation_image,
    "preview rejected image": output.preview_rejected_image,
    "preview recovery intervals": output.preview_recovery_intervals,
    "preview alternating bytes": output.preview_alternating_bytes,
    "preview same producer": output.preview_producer_suffix,
    "preview unicode suffix": output.preview_producer_suffix,
    "preview unicode replacement": output.preview_producer_suffix,
    "preview unicode lines": output.preview_unicode_lines,
    "preview huge line": output.preview_large_output,
    "preview tiny events": output.preview_large_output,
    "preview many tiny events": output.preview_large_output,
    "preview redraw": output.preview_large_output,
    "preview ansi redraw": output.preview_ansi_redraw,
    "emit stdout": output.emit_stdout,
    "redraw across polls": output.redraw_across_polls,
    "stress redraws": output.stress_redraws,
    "language error": output.language_error,
    "complete silently": output.complete_silently,
    "record intervals": output.recording_intervals,
    "emit console kinds": output.emit_console_kinds,
    "emit image": output.emit_image,
    "emit image before completion": output.emit_image_before_completion,
    "emit output and image before completion": output.emit_image_before_completion,
    # Input
    "request input": input.request_input,
    "preview prompt": input.preview_prompt,
    "request input after timeout": input.request_input_after_timeout,
    "input without request then request input": input.input_without_request_then_request_input,
    "input without request": input.input_without_request,
    "input length without request": input.input_without_request,
    "read fd 0 directly": input.read_fd_zero_directly,
    "request input while idle": input.request_input_while_idle,
    # Preparation
    "resolve python while idle": preparation.resolve_python_while_idle,
    "report runtime R resolution failure": preparation.report_runtime_r_resolution_failure,
    "report managed R requirement": preparation.report_managed_r_requirement,
    "report raw R library bytes": preparation.report_raw_r_library_bytes,
    "fail next r preparation": preparation.fail_next_r_preparation,
    "fail next r preparation after output": preparation.fail_next_r_preparation_after_output,
    "report managed python activation": preparation.report_managed_python_activation,
    # Lifecycle
    "interrupt": lifecycle.interrupt,
    "stall": lifecycle.stall,
    "close sideband with unread shutdown": lifecycle.close_sideband_with_unread_shutdown,
    "exit unexpectedly": lifecycle.exit_unexpectedly,
    "exit after output": lifecycle.exit_unexpectedly,
    "exit zero": lifecycle.exit_zero,
    "wait for stdin close": lifecycle.wait_for_stdin_close,
    "set controlled restart state": lifecycle.set_controlled_restart_state,
    "inspect controlled restart state": lifecycle.inspect_controlled_restart_state,
    "shutdown output checkpoints": lifecycle.shutdown_output_checkpoints,
    "complete before restart checkpoint": lifecycle.complete_before_restart_checkpoint,
    "report process group": lifecycle.report_process_group,
    "complete after release": lifecycle.complete_after_release,
    "output then complete after release": lifecycle.output_then_complete_after_release,
    # Failures
    "fail sideband during shutdown": failures.fail_sideband_during_shutdown,
    "violate protocol": failures.violate_protocol,
    "violate protocol after stdout": failures.violate_protocol_after_stdout,
    "unexpected input receipt after stdout": failures.unexpected_input_receipt_after_stdout,
    "malformed sideband after stdout": failures.malformed_sideband_after_raw_output,
    "malformed sideband after stderr": failures.malformed_sideband_after_raw_output,
    "force stop after raw stdout": failures.force_stop_after_raw_output,
    "force stop after raw stderr": failures.force_stop_after_raw_output,
    "preview invalid oversized image": failures.preview_invalid_oversized_image,
    "exit after invalid stdout": failures.exit_after_invalid_raw_output,
    "exit after invalid stderr": failures.exit_after_invalid_raw_output,
    # Probes
    "stall accepted relay shutdown": probes.stall_accepted_relay_shutdown,
    "stall with stopped relay": probes.stall_with_stopped_relay,
    "kill relay and remain live": probes.kill_relay_and_remain_live,
    "start background stderr": probes.start_background_stderr,
    "start background sideband": probes.start_background_sideband,
    "start partial sideband descendant": probes.partial_sideband_descendant,
    "exit after partial sideband descendant": probes.partial_sideband_descendant,
    "wait after readable frame and partial tail": probes.wait_after_readable_frame_and_partial_tail,
    "complete before partial sideband descendant": probes.complete_before_partial_sideband_descendant,
    "probe sandbox": probes.probe_sandbox,
}


def dispatch(context: WorkerContext, source: str) -> LoopAction | None:
    if source in EXACT_HANDLERS:
        return EXACT_HANDLERS[source](context, source)
    if source.startswith("check response gate: "):
        return probes.check_response_gate(context, source)
    if source.startswith("checkpoint "):
        return probes.checkpoint(context, source)
    if source.startswith("wait for interrupt: "):
        return lifecycle.wait_for_interrupt(context, source)
    if source.startswith("exit after output: "):
        return lifecycle.exit_unexpectedly(context, source)
    if source.startswith("hold after output: "):
        return lifecycle.hold_after_output(context, source)
    if source.startswith("stall: "):
        return lifecycle.stall_operation(context, source)
    if source.startswith("preview recovery cell "):
        return output.preview_recovery_cell(context, source)
    if source.startswith("start background sideband: "):
        return probes.start_background_sideband(context, source)
    if source.startswith("stall with detached stdin: "):
        return probes.stall_with_detached_stdin(context, source)
    if source.startswith("echo "):
        return output.echo(context, source)
    raise AssertionError(f"unsupported Zod command: {source}")
