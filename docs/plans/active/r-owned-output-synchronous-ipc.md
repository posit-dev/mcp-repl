# R-Owned Output Synchronous IPC

## Summary

- Move R-owned observable text onto ordered worker-to-server IPC frames.
- Keep raw stdout and stderr capture for unowned output from child processes and direct file-descriptor writes.

## Status

- State: active
- Last updated: 2026-05-07
- Current phase: phase 3 pending

## Current Direction

- Add broader ordering and backpressure coverage, then remove race-tolerant
  handling that only applied to R-owned raw pipe output.

## Long-Term Direction

- R-owned text, readline facts, plots, and session termination should share one ordered IPC stream.
- The server should not need timing reconstruction for output that the R backend owns.

## Phase Status

- Phase 0: completed - protocol frame and synchronous worker IPC writer.
- Phase 1: completed - append `output_text` into server timelines.
- Phase 2: completed - route R console callbacks and readline echo through `output_text`.
- Phase 3: pending - remove race-tolerant handling that only existed for R-owned output.

## Locked Decisions

- Use `output_text { stream, data_b64 }` for worker-owned text.
- Do not add per-output acknowledgements, byte matching, hashes, or alignment heuristics.
- Keep raw stdout and stderr readers for output the worker protocol does not own.

## Open Questions

- None for the current protocol slice.

## Next Safe Slice

- Add focused ordering coverage for R-owned stdout, stderr, readline echo, plots,
  direct file-descriptor writes, and large-output backpressure.

## Stop Conditions

- Stop and ask before changing the public reply shape.
- Stop and ask before removing raw stdout or stderr capture.

## Decision Log

- 2026-05-07: Start with the protocol and synchronous worker writer so later routing can be reviewed separately from server timeline consumption.
- 2026-05-07: Completed the protocol foundation without changing existing raw stdout/stderr capture or asynchronous non-output IPC sends.
- 2026-05-07: Completed server-side `output_text` consumption by decoding worker-owned text in the IPC reader and appending it through the existing live output capture path.
- 2026-05-07: Routed R console callbacks and readline echo through ordered IPC
  output text, leaving raw stdout and stderr capture for unowned fd output.
