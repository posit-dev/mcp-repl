# Future Work: Server Backend Boundary

## Summary

The server should be mostly blind to how the worker implements a selected REPL
runtime. It should spawn the worker, send accepted input over stdin, capture
stdout/stderr, consume sideband facts, assemble the output timeline, and finalize
the MCP reply.

Backend-specific execution semantics should live on the worker side. Any current
R/Python branching in server request handling should be treated as abstraction
leakage unless it is only launch-time configuration or user-facing tool
description selection.

## Why This Matters

The intended contract is a narrow server/worker boundary:

- stdin carries user input to the worker,
- stdout/stderr carry visible worker text back to the server,
- sideband IPC carries structural facts such as prompts, readline results,
  images, request completion, and session end,
- the server formats replies from those streams without understanding the
  runtime's internal implementation.

Keeping that boundary clear makes it easier to add or change runtimes without
teaching the server backend-specific behavior.

## Current Drift To Revisit

- `src/backend.rs` exposes a `Backend` enum that is used by server-side setup.
- `src/server.rs` selects `repl` tool descriptions by backend and oversized-output
  mode.
- `src/worker_process.rs` contains server-side backend driver branching for R vs
  Python behavior.

Some of this may be necessary during launch or documentation rendering, but it
should not leak into steady-state server request semantics.

## Backend Branching Audit

As of 2026-05-06, production server-side backend branching falls into these
buckets:

- `src/main.rs` parses `Backend` from CLI/env and renders install-time config.
  This is expected user-facing interpreter selection.
- `src/server.rs` selects one RMCP tool server per `(Backend,
  OversizedOutputMode)` so each `repl` tool can have a static doc string. This
  is documentation wiring, not request execution policy, and should move with
  `docs/futurework/composable-tool-descriptions.md`.
- `src/worker_process.rs` selects a `BackendDriver` at `WorkerManager`
  creation. The driver owns request payload framing, Python's request-start
  sideband nudge, interrupt behavior, completion waiting, and backend-info
  startup tolerance. This is acceptable only as a server-side adapter until the
  worker can advertise these narrow capabilities.
- `src/worker_process.rs` also branches at spawn time between the in-binary R
  worker and the Python PTY process. This is launch-time configuration.
- On Windows, `src/worker_process.rs` prepares a sandbox launch only for R
  because the Python backend currently reports a Unix-PTY requirement. This is
  launch/platform gating, not steady-state request semantics.
- The main steady-state leak found by this audit was the former
  `WorkerManager::should_settle_multiline_r_timeout()` branch. The behavior now
  sits behind the backend driver as timeout-output-settle policy; a later
  cleanup should replace that adapter method with worker-advertised metadata or
  move the need behind the worker protocol.

## Intended Direction

- Treat `--interpreter r|python` as user-facing worker selection.
- Keep backend-specific runtime behavior in the worker process.
- Keep the server's steady-state contract generic: stdin in, stdout/stderr plus
  sideband facts out.
- Prefer worker-advertised capabilities or narrow launch-time metadata over
  server-side branching on backend.
- Coordinate tool-description cleanup with
  `docs/futurework/composable-tool-descriptions.md`.

## Narrow Capability Metadata

If the server needs to preserve a backend-specific behavior while removing a
direct R/Python branch, the worker should advertise the smallest capability that
explains that behavior. Capability names should describe server-visible protocol
semantics, not implementation language.

Initial candidates:

- `supports_images`: already present in `backend_info`; controls whether image
  events are expected.
- `input_framing`: whether server stdin payloads are raw text or length-framed
  records.
- `stdin_write_control`: whether a request-start sideband message is needed
  before stdin bytes are written.
- `backend_info_startup_timeout`: whether startup may continue after a short
  backend-info timeout.
- `timeout_output_settle`: whether a timed-out request needs an additional
  output-settle window before the server returns the timeout reply.

This metadata should stay startup-only. Request handling should branch on the
capability value it already received, not on the selected backend.

## Non-Goals

- Removing the R/Python interpreter selector.
- Redesigning the sideband protocol in this item.
- Changing the public `repl` or `repl_reset` tool contract before the boundary
  cleanup is designed.
