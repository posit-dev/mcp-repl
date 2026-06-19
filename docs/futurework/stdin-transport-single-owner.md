# Stdin Transport Single-Owner Refactor

## Summary

A Windows bug exposed a broader design issue in the embedded worker model:
managed turn input should have a single owner inside the worker process.

This repo has already pulled forward a narrow mitigation from that future work: on Windows, pause the worker's background stdin reader while a request is active so another runtime is not competing with a blocked reader on fd `0`.

The main protocol follow-up has been pulled into the current worker contract:
accepted input is queued through sideband IPC and runtime stdin surfaces are
worker-owned implementation details. The remaining value of this note is to keep
future interpreters on that same single-owner path.

## Why This Matters

- The Windows `reticulate` hang was a concrete symptom of stdin ownership problems in the worker model.
- The problem is not "piped stdin is always broken". The hang showed up when another thread was already blocked on the same stdin pipe.
- Future embedded interpreters can run into similar issues if managed input
  ownership drifts back toward shared raw process stdin.
- The current worker protocol carries accepted input in `turn_start` or
  same-turn `turn_input` and lets the worker own the runtime boundary.

## Current Scope

Built-in and custom workers should keep managed turn input in a worker-owned
queue. The worker may expose that queue through `ReadConsole`, `PyOS_Readline`,
a managed `sys.stdin`, direct fd shims, a PTY writer, or another runtime bridge,
but the server-side request path should stay opaque.

## Intended Transport Model

- Treat the v3 `turn_start` and `turn_input` payloads as the managed input
  transport.
- Do not add framing headers or other synthetic protocol markers to raw process
  stdin.
- Keep request metadata and queued input on sideband IPC.
- Let the worker own when the current input payload is complete.
- For line-oriented runtimes such as embedded R, expect a single logical request to be satisfied across multiple `readline` or `ReadConsole` calls.

Raw process stdin may still exist for launch, terminal behavior, or child
process policy, but it is not the managed request payload transport.

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

- Keep IPC-queued input as the primary managed worker payload transport.
- Make runtime stdin ownership explicit per backend instead of relying on
  scattered server-side branching.
- Avoid a permanently blocked background stdin reader while embedded runtimes
  may also inspect or wrap fd `0`.
- Prefer demand-driven reads from stdin, or another single-owner design, so
  future interpreters can fit the same transport model.
