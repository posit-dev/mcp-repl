# Built-In R Worker Turn Boundary Simplification

## Summary

This note is superseded by the IPC-queued opaque worker protocol documented in
`docs/worker_sideband_protocol.md`.

## Status

Status: superseded.

Historical baseline: Server-inferred completion is no longer the intended direction.
The old simplification target was worker-emitted `idle` or `session_end`; the
current protocol keeps that fail-closed direction and adds `stdin_wait` for
stdin-continuation reply boundaries.

The current protocol contract is:

- accepted input enters the worker through `turn_start`,
- stdin-style continuation input uses `turn_input`,
- the worker owns the input queue and runtime placement,
- successful same-worker reply boundaries are reported with `idle` or
  `stdin_wait`,
- `session_end` is terminal for any active turn,
- the server does not infer completion from prompt text, raw process stdin
  writes, PTY state, stdout/stderr, or timing.

Keep this file only as historical context for why the protocol moved request
boundaries into the worker.

## Intended Direction

Future changes should update `docs/worker_sideband_protocol.md` first, then
adjust this historical note only if that context would otherwise mislead
readers.
