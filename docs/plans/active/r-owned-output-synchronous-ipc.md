# R-Owned Output Synchronous IPC

## Summary

- Move R-owned observable text onto ordered worker-to-server IPC frames.
- Keep raw stdout and stderr capture for unowned output from child processes and direct file-descriptor writes.

## Status

- State: active
- Last updated: 2026-05-08
- Current phase: phase 3 completed

## Current Direction

- Remove race-tolerant handling that only applied to R-owned raw pipe output.

## Long-Term Direction

- R-owned text, readline facts, plots, and session termination should share one ordered IPC stream.
- The server should not need timing reconstruction for output that the R backend owns.

## Phase Status

- Phase 0: completed - protocol frame and synchronous worker IPC writer.
- Phase 1: completed - append `output_text` into server timelines.
- Phase 2: completed - route R console callbacks and readline echo through `output_text`.
- Phase 3: completed - R-shaped raw stdout is no longer trimmed by files-mode
  sideband-first carryover; R-owned `output_text` echo still has source-aware
  carryover for drain boundaries. R completion prompts are now appended from
  framed prompt facts instead of stripping prompt-shaped raw stdout. A public
  files-mode regression covers raw child stdout that exactly matches a later
  R-owned prompt/input echo.
- Phase 4: planned - evaluate a bounded pre-input drain gate. `stdin_write_ack`
  only means the worker has installed request metadata before raw stdin bytes
  arrive; any raw-output drain gate should be a separate request-boundary
  protocol step.

## Locked Decisions

- Use `output_text { stream, data_b64, is_continuation }` for worker-owned text.
- Do not add per-output acknowledgements, byte matching, hashes, or alignment heuristics.
- A pre-input drain gate, if added, must be request-boundary coordination with a
  bounded server-side drain budget, not per-output acknowledgement.
- Keep raw stdout and stderr readers for output the worker protocol does not own.

## Open Questions

- What exact protocol shape should gate delivery of the next stdin payload while
  the server drains raw stdout/stderr from the previous boundary?
- Should the initial drain budget be 200 ms, and should it apply to both R and
  Python?

## Next Safe Slice

- Review remaining prompt-fallback cleanup in `src/worker_process.rs`; keep raw
  pipe fallback separate from source-aware IPC echo carryover.
- Design the smallest pre-input drain-gate slice: the worker pauses before
  consuming new input, the server drains raw stdout/stderr for a fixed budget,
  then input delivery continues. Child output after that boundary belongs to the
  next response.

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
  prompts no longer trim later raw stdout. The backend now records the expected
  echo source on `readline_result`, so backend-owned `output_text` echo can
  carry across drain boundaries without deriving the source from prompt
  spelling.
- 2026-05-08: Stopped treating R raw stdout that equals the primary prompt as
  the completion prompt. The server now appends the R completion prompt from
  framed IPC facts, including interrupt-drained completions, while leaving
  prompt-shaped child stdout visible.
- 2026-05-08: Completed the files-mode prompt/readline cleanup slice with a
  public regression proving raw child stdout that exactly matches a later
  R-owned prompt/input echo remains visible. No runtime change was needed
  because same-drain and carryover echo collapse already require matching
  `readline_result` source facts.
- 2026-05-08: Kept ACK-gated input delivery open as a request-boundary tool, not
  a per-output ACK. The useful shape is: before the worker consumes the next
  input, the server gets a bounded opportunity to drain raw stdout/stderr from
  the previous boundary, with a hard stop around a small budget such as 200 ms.
  Child output that arrives after the gate belongs to the next response. This
  may simplify timeline reconstruction for child-process and direct-fd output
  without changing the R-owned `output_text` path.
