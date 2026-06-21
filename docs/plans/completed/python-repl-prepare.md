# Python repl_prepare Tool

## Summary

Implemented a Python-only `repl_prepare` MCP tool that prepares the Python runtime for later `repl` calls. It replaces Python `repl_reset`; plain Python reset remains available through `\u0004` in `repl` input. R keeps `repl_reset` unchanged.

## Final Interface

```text
repl_prepare(requirements?, python?)
```

- Omit both arguments to prepare the default ephemeral uv-managed Python with `["numpy"]`.
- `requirements: {}` prepares a stdlib-only ephemeral uv-managed Python.
- `requirements.packages` is exact and does not add defaults.
- `requirements.python_version` is forwarded to `uv` as supplied, including bare versions such as `3.11`.
- `python.executable` selects an absolute Python executable as-is.
- `python.venv` selects an absolute virtual environment directory and resolves it to `bin/python` on Unix or `Scripts/python.exe` on Windows.
- Provide at most one of top-level `requirements` or `python`.
- Inside `python`, provide exactly one of `executable` or `venv`.
- Unknown fields are rejected. There is no `timeout_ms`.

## Semantics

- `repl_prepare` does not execute user code, import packages, attach modules, or create aliases.
- Explicit `python` selections are non-mutating and receive no default packages.
- Preparation may preserve or replace the current session.
- Replacement discards previous variables, imports, pending output, and active work.
- Replies state whether the session was unchanged or replaced and whether pending work was discarded.
- Python `repl_prepare` is registered only when `uv` is on `PATH`; otherwise Python exposes neither `repl_prepare` nor `repl_reset`.

## Implementation Notes

- The requirements resolver is modeled after `reticulate::py_require()` / `uv_get_or_create_env()` and uses `uv tool run --isolated`.
- `uv` is required; there is no bootstrap and no fallback to pip, venv, or reticulate.
- Prepared Python workers carry both the selected executable and its probed module search path so embedded Python uses the selected environment instead of the server's base Python site-packages.
- Future manager selectors such as `python.conda` or `python.pixi` remain unimplemented until activation semantics are designed.

## Verification Coverage

- Python tool listing includes `repl_prepare` and excludes `repl_reset` when a test `uv` is on `PATH`.
- Python tool listing excludes both `repl_prepare` and `repl_reset` when `uv` is missing.
- R tool listing remains unchanged.
- Default `{}` preparation provides `numpy`.
- `requirements: {}` is stdlib-only.
- `requirements.packages` does not add defaults.
- Invalid XOR shapes, relative Python paths, and unknown fields are rejected through the public tool surface.
- Absolute executable and venv paths are accepted.
- `python_version` values such as `3.11`, `3.11.4`, and `>=3.11,<3.13` are accepted and forwarded.
- Preparation can replace an active session and reports discarded work.
- Default requirements preserve the current Python session when the current runtime already provides the requested distribution metadata.
- Matching explicit Python executable preparation preserves the existing session.

## Decision Log

- 2026-06-21: Chose a separate `repl_prepare` tool rather than adding requirements to `repl`.
- 2026-06-21: Chose a Python-specific schema because each MCP server instance is specialized to one interpreter.
- 2026-06-21: Chose top-level `requirements` XOR `python`, with `python.executable` XOR `python.venv`.
- 2026-06-21: Chose `uv` as the only Python requirements resolver for this feature.
- 2026-06-21: Chose default Python packages `["numpy"]`.
- 2026-06-21: Chose to omit `timeout_ms` from `repl_prepare`.
