# R-Owned Output Synchronous IPC

## Summary

- Move R-owned observable text onto ordered worker-to-server IPC frames.
- Keep raw stdout and stderr capture for unowned output from child processes and direct file-descriptor writes.

## Status

- State: active
- Last updated: 2026-05-08
- Current phase: phase 3 in progress

## Current Direction

- Remove race-tolerant handling that only applied to R-owned raw pipe output.

## Long-Term Direction

- R-owned text, readline facts, plots, and session termination should share one ordered IPC stream.
- The server should not need timing reconstruction for output that the R backend owns.

## Phase Status

- Phase 0: completed - protocol frame and synchronous worker IPC writer.
- Phase 1: completed - append `output_text` into server timelines.
- Phase 2: completed - route R console callbacks and readline echo through `output_text`.
- Phase 3: in progress - R prompt carryover removed from the files-mode
  sideband-first fallback; remaining carryover is for non-R raw prompt echo.

## Locked Decisions

- Use `output_text { stream, data_b64, is_continuation }` for worker-owned text.
- Do not add per-output acknowledgements, byte matching, hashes, or alignment heuristics.
- Keep raw stdout and stderr readers for output the worker protocol does not own.

## Open Questions

- None for the current protocol slice.

## Next Safe Slice

- Review remaining prompt-fallback cleanup in `src/worker_process.rs` and keep
  any fallback documented as non-R raw pipe behavior.

## Stop Conditions

- Stop and ask before changing the public reply shape.
- Stop and ask before removing raw stdout or stderr capture.

## Decision Log

- 2026-05-07: Start with the protocol and synchronous worker writer so later routing can be reviewed separately from server timeline consumption.
- 2026-05-07: Completed the protocol foundation without changing existing raw stdout/stderr capture or asynchronous non-output IPC sends.
- 2026-05-07: Completed server-side `output_text` consumption by decoding worker-owned text in the IPC reader and appending it through the existing live output capture path.
- 2026-05-07: Routed R console callbacks and readline echo through ordered IPC
  output text, leaving raw stdout and stderr capture for unowned fd output.
- 2026-05-08: Added focused ordering and raw-output fallback coverage for
  R-owned stdout, stderr, readline echo, plots, direct file-descriptor writes,
  child output, and large output.
- 2026-05-08: Narrowed files-mode sideband-first echo carryover so ordinary R
  prompts no longer trim later raw stdout. Python-style raw prompt echo remains
  eligible for carryover.
