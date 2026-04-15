# Stdin Transport Single-Owner Refactor

## Summary

A Windows bug exposed a broader design issue in the embedded worker model: stdin should have a single owner inside the worker process.

This repo has already pulled forward a narrow mitigation from that future work: on Windows, pause the worker's background stdin reader while a request is active so another runtime is not competing with a blocked reader on fd `0`.

The remaining follow-up is broader than that point fix. We still need to tighten the general stdin transport model so future interpreters fit the same design cleanly and stdin ownership is structural rather than incidental.

## Why This Matters

- The Windows `reticulate` hang was a concrete symptom of stdin ownership problems in the worker model.
- The problem is not "piped stdin is always broken". The hang showed up when another thread was already blocked on the same stdin pipe.
- Future embedded interpreters can run into similar issues if worker stdin ownership drifts again.
- The current embedded R stdin path still has implementation drift from the intended worker/server contract.

## Current Scope

This repo now avoids the immediate Windows deadlock by stopping the background stdin reader from blocking on fd `0` while a request is running.

That mitigation should be treated as an initial slice, not the completion of this item:

- It is Windows-only.
- It reduces simultaneous stdin readers during active requests.
- It does not yet make stdin ownership explicit end-to-end.
- It does not remove the embedded R framing layer.
- It does not yet establish the final request-envelope split between raw stdin payloads and IPC metadata.

We still want to keep stdin as the primary request channel for worker payloads and address the broader stdin ownership and transport shape in a dedicated follow-up refactor.

## Intended Transport Model

- Treat worker stdin as the real raw input stream delivered to the interpreter.
- Do not add framing headers or other synthetic protocol markers to stdin.
- Mirror request metadata over IPC instead: request start, expected input payload, completion, and other turn/state signals.
- Let the worker use the IPC envelope to know when the current stdin payload is complete, while still feeding raw stdin through the interpreter-facing path.
- For line-oriented runtimes such as embedded R, expect a single logical request to be satisfied across multiple `readline` or `ReadConsole` calls.

The current embedded R implementation uses framed stdin messages to preserve request boundaries on a long-lived stream. That is implementation drift from the intended model and should be removed as part of this follow-up.

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
- Refactor the worker so stdin has a single owner.
- Avoid a permanently blocked background stdin reader while embedded runtimes may also inspect or wrap fd `0`.
- Treat the current active-request pause as a temporary safety rail, not the final transport architecture.
- Remove stdin framing for the embedded R worker and rely on IPC for request envelope metadata instead.
- Prefer demand-driven reads from stdin, or another single-owner design, so future interpreters like Julia can fit the same transport model.
