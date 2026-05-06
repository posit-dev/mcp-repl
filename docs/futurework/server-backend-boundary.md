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

## Intended Direction

- Treat `--interpreter r|python` as user-facing worker selection.
- Keep backend-specific runtime behavior in the worker process.
- Keep the server's steady-state contract generic: stdin in, stdout/stderr plus
  sideband facts out.
- Prefer worker-advertised capabilities or narrow launch-time metadata over
  server-side branching on backend.
- Coordinate tool-description cleanup with
  `docs/futurework/composable-tool-descriptions.md`.

## Non-Goals

- Removing the R/Python interpreter selector.
- Redesigning the sideband protocol in this item.
- Changing the public `repl` or `repl_reset` tool contract before the boundary
  cleanup is designed.
