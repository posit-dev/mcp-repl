# Stdin Transport Single-Owner Refactor

## Summary

A Windows bug exposed a broader design issue in the embedded worker model: stdin should have a single owner inside the worker process.

The immediate failure was Windows-specific. Embedded Python can hang during `Py_InitializeEx()` when the process has a live stdin pipe and another thread is already blocked reading that same pipe. But the follow-up refactor should not be Windows-specific. The goal is to tighten the general stdin transport model so future interpreters fit the same design cleanly.

## Why This Matters

- `reticulate::py_config()` and `reticulate::py_help()` can hang in the embedded R worker on Windows.
- The problem is not "piped stdin is always broken". The hang only shows up when another thread is already blocked on the same stdin pipe.
- This affects future embedded interpreters too, not just `reticulate`.
- The current embedded R stdin path has implementation drift from the intended worker/server contract.

## Current Scope

This repo's current sandbox PR intentionally does not change the general worker/server transport model. We want to keep stdin as the primary request channel for worker payloads and address stdin ownership in a dedicated follow-up refactor.

## Intended Transport Model

- Treat worker stdin as the real raw input stream delivered to the interpreter.
- Do not add framing headers or other synthetic protocol markers to stdin.
- Mirror request metadata over IPC instead: request start, expected input payload, completion, and other turn/state signals.
- Let the worker use the IPC envelope to know when the current stdin payload is complete, while still feeding raw stdin through the interpreter-facing path.
- For line-oriented runtimes such as embedded R, expect a single logical request to be satisfied across multiple `readline`/`ReadConsole` calls.

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
- Remove stdin framing for the embedded R worker and rely on IPC for request envelope metadata instead.
- Prefer demand-driven reads from stdin, or another single-owner design, so future interpreters like Julia can fit the same transport model.
