# Built-In R Worker Turn Boundary Simplification

## Summary

This note is superseded by the IPC-queued opaque worker protocol documented in
`docs/worker_sideband_protocol.md`.

## Status

Status: superseded.

Historical baseline: Server-inferred completion is no longer the intended direction. The old simplification target was worker-emitted `idle` or
`session_end`; that was superseded by protocol v4's single worker-emitted
`input_wait` boundary. The current success boundary is worker-emitted `input_wait` or `session_end`.

The current protocol contract is:

- accepted input enters the worker through `turn_start`,
- the worker owns the input queue and runtime placement,
- successful same-worker reply boundaries are reported with `input_wait`,
- `session_end` is terminal for any active turn,
- the server does not infer completion from prompt text, raw process stdin
  writes, PTY state, stdout/stderr, or timing.

Keep this file only as historical context for why the protocol moved request
boundaries into the worker.

## Intended Direction

Future changes should update `docs/worker_sideband_protocol.md` first, then
adjust this historical note only if that context would otherwise mislead
readers.
