# Windows Stdin Single-Owner Refactor

## Summary

Windows embedded Python can hang during `Py_InitializeEx()` when the process has a live stdin pipe and another thread is already blocked reading that same pipe. In the current worker design, the background stdin reader and embedded runtimes can both touch fd `0`, which is unsafe on Windows.

## Why This Matters

- `reticulate::py_config()` and `reticulate::py_help()` can hang in the embedded R worker on Windows.
- The problem is not "piped stdin is always broken". The hang only shows up when another thread is already blocked on the same stdin pipe.
- This affects future embedded interpreters too, not just `reticulate`.

## Current Scope

This repo's current sandbox PR intentionally does not change the general worker/server transport model. We want to keep stdin as the primary request channel for worker payloads and address the Windows stdin ownership problem in a dedicated follow-up refactor.

## Observed Call Path

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
- Prefer demand-driven reads from stdin, or another single-owner design, so future interpreters like Julia can fit the same transport model.
