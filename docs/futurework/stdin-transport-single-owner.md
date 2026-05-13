# Stdin Transport Single-Owner Refactor

## Summary

A Windows bug exposed a broader design issue in the embedded worker model: stdin should have a single owner inside the worker process.

This repo has already pulled forward a narrow mitigation from that future work: on Windows, pause the worker's background stdin reader while a request is active so another runtime is not competing with a blocked reader on fd `0`.

The remaining follow-up is broader than that point fix. We still need to tighten the general stdin transport model so future interpreters fit the same design cleanly and stdin ownership is structural rather than incidental.

## Why This Matters

- The Windows `reticulate` hang was a concrete symptom of stdin ownership problems in the worker model.
- The problem is not "piped stdin is always broken". The hang showed up when another thread was already blocked on the same stdin pipe.
- Future embedded interpreters can run into similar issues if worker stdin
  ownership drifts again.
- The current R and Python paths now keep stdin raw and use sideband metadata
  for request boundaries, but their in-worker stdin ownership differs.

## Current Scope

This repo now uses raw stdin for worker payloads and sideband IPC for request
metadata:

- R worker mode owns stdin in a worker-side reader thread. The server announces
  the payload byte length on sideband IPC; the worker reader consumes exactly
  that many raw bytes and submits them to embedded R.
- Python worker mode lets CPython own stdin. The server announces request
  metadata on sideband IPC, waits for `stdin_write_ack`, then writes raw bytes
  for CPython's interactive loop to consume.

The remaining follow-up is to make this ownership split more explicit in code
and reduce server-side backend branching around request metadata.

## Intended Transport Model

- Treat worker stdin as the real raw input stream delivered to the interpreter.
- Do not add framing headers or other synthetic protocol markers to stdin.
- Mirror request metadata over IPC instead: request start, expected input payload, completion, and other turn/state signals.
- Let the worker use the IPC envelope to know when the current stdin payload is complete, while still feeding raw stdin through the interpreter-facing path.
- For line-oriented runtimes such as embedded R, expect a single logical request to be satisfied across multiple `readline` or `ReadConsole` calls.

The current embedded worker implementation keeps stdin raw and preserves request
boundaries with IPC metadata.

## Observed Windows Failure

- `reticulate` calls `Py_InitializeEx(0)`.
- CPython initializes `sys.stdin` in `Python/pylifecycle.c`.
- That path wraps fd `0` via `_io.FileIO`.
- On Windows, that wrapper path can hang when another thread is already blocked reading the same stdin pipe.

## Local Repro Notes

The following patterns reproduced locally on Windows:

- Standalone embedded Python init succeeds with a piped stdin when no thread is already reading stdin.
- The same init hangs when another thread is blocked on `stdin.readline()`.
- Plain Python `io.FileIO(0, "rb", closefd=False)` shows the same behavior under the same conditions.
- `_setmode(0, O_BINARY)` and `_isatty(0)` do not hang in that setup, but `_fstat64(0, ...)` does.

## Intended Follow-Up

- Keep stdin as the primary worker payload transport.
- Make stdin ownership explicit per backend instead of relying on scattered
  server-side branching.
- Avoid a permanently blocked background stdin reader while embedded runtimes
  may also inspect or wrap fd `0`.
- Prefer demand-driven reads from stdin, or another single-owner design, so
  future interpreters can fit the same transport model.
