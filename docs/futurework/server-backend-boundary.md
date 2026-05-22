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
- `output_text` sideband frames carry worker-owned text back to the server,
- raw stdout/stderr carry unowned visible text from child processes or direct
  file-descriptor writes,
- sideband IPC also carries structural facts such as prompts, readline results,
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

As of 2026-05-09, production server-side backend branching falls into these
buckets:

- `src/main.rs` parses `Backend` from CLI/env and renders install-time config.
  This is expected user-facing interpreter selection.
- `src/server.rs` selects one RMCP tool server per `(Backend,
  OversizedOutputMode)` so each `repl` tool can have a static doc string. This
  is documentation wiring, not request execution policy, and should move with
  `docs/futurework/composable-tool-descriptions.md`.
- `src/worker_process.rs` selects a `BackendDriver` at `WorkerManager`
  creation. The driver owns backend-specific adapter details such as Python
  newline normalization, interrupt behavior, completion waiting, and worker
  startup tolerance. This is acceptable only as a server-side adapter until the
  worker protocol can make these narrow capabilities generic.
- `src/worker_process.rs` also branches at spawn time to configure R worker mode
  or Python worker mode in the same `mcp-repl` executable. Python launch setup
  additionally resolves the selected interpreter executable and loadable
  CPython library. This is launch-time configuration.
- On Windows, `src/worker_process.rs` still has platform-specific launch setup,
  but Python no longer depends on a Unix PTY.
- The main steady-state leak found by this audit was the former
  `WorkerManager::should_settle_multiline_r_timeout()` branch. The behavior now
  sits behind the backend driver as timeout-output-settle policy; a later
  cleanup should replace that adapter method with worker-advertised metadata or
  move the need behind the worker protocol.

## Intended Direction

- Treat `--interpreter r|python` as user-facing worker selection.
- Keep backend-specific runtime behavior in the worker process.
- Keep the server's steady-state contract generic: stdin in, worker-owned
  `output_text` plus raw stdout/stderr and sideband facts out.
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

- `supports_images`: reported at worker startup; controls whether image events
  are expected.
- `worker_ready_startup_timeout`: whether startup may continue after a short
  worker-ready timeout.
- `timeout_output_settle`: whether a timed-out request needs an additional
  output-settle window before the server returns the timeout reply.

This metadata should stay startup-only. Request handling should branch on the
capability value it already received, not on the selected backend.

## Non-Goals

- Removing the R/Python interpreter selector.
- Redesigning the sideband protocol in this item.
- Changing the public `repl` or `repl_reset` tool contract before the boundary
  cleanup is designed.
