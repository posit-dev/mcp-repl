# Request Output Finalization

## Summary

- Replaced scattered request completion, output settling, marker insertion, and UTF-8 tail recovery decisions with explicit request-output finalization policy.
- Kept the worker sideband protocol unchanged; the server remains responsible for reconstructing visible output from IPC and raw streams.

## Status

- State: completed
- Last updated: 2026-06-26
- Current phase: completed

## Current Direction

- Timeline markers are pure ordering facts that do not implicitly seal UTF-8 tails.
- Request outcome policy is server-side, deadline-aware, and outcome-specific.
- UTF-8 tail details stay inside output timeline/finalization code instead of being controlled by marker side effects.

## Long-Term Direction

- The output timeline owns byte assembly and event ordering.
- Request lifecycle owns only the request outcome: completed, timed out, session ended, or failed.
- Presentation code drains output according to explicit policies instead of relying on marker side effects.

## Phase Status

- Phase 0: completed - architecture issue identified.
- Phase 1: completed - public regressions and finalization refactor.
- Phase 2: completed - required validation passed.

## Locked Decisions

- Do not add a worker protocol flush/ack message for this issue.
- Treat `timeout_ms` as a hard public response deadline.
- Preserve timeout replies that expose already-buffered visible output behind an incomplete UTF-8 head by explicit timeout policy.

## Open Questions

- None.

## Next Safe Slice

- None.

## Stop Conditions

- Reassess if a future change requires worker-side output flush acknowledgements.
- Reassess if timeout replies can no longer expose visible output already buffered before the timeout.

## Decision Log

- 2026-06-26: Chose a server-side finalizer because raw stdout/stderr arrival is not synchronized by worker IPC.
- 2026-06-26: Chose pure marker insertion so `RequestBoundary` and `InputWait` cannot accidentally seal or escape UTF-8 bytes.
- 2026-06-26: Kept timeout drains deadline-capped and used explicit visible-output sealing for timeout presentation.
- 2026-06-26: Completed required validation: `cargo check`, `cargo build`, integration tests, clippy, full Rust tests, and `cargo +nightly fmt`.
