# Python repl_prepare Tool

## Summary

Implemented a Python-only `repl_prepare` MCP tool that prepares the Python runtime for later `repl` calls. It replaces Python `repl_reset`; plain Python reset remains available through `\u0004` in `repl` input. R keeps `repl_reset` unchanged.

## Final Interface

```text
repl_prepare(requirements?, python?)
```

- mcp-repl keeps a persistent managed-Python requirements manifest, initially `["numpy"]`.
- Omit both arguments to realize the current manifest.
- `requirements: {}` is a no-op manifest update, then realization of the current manifest.
- `requirements.packages` updates the manifest according to `requirements.action`.
- `requirements.action` is `"add"`, `"remove"`, or `"set"` and defaults to `"add"`.
- `requirements.restart` is `"if_needed"`, `"no"`, or `"yes"` and defaults to `"if_needed"`.
- `requirements.python_version` is forwarded to `uv` as supplied, including bare versions such as `3.11`.
- `python.executable` selects an absolute Python executable as-is.
- `python.venv` selects an absolute virtual environment directory and resolves it to `bin/python` on Unix or `Scripts/python.exe` on Windows.
- Provide at most one of top-level `requirements` or `python`.
- Inside `python`, provide exactly one of `executable` or `venv`.
- Unknown fields are rejected. There is no `timeout_ms`.

## Semantics

- `repl_prepare` does not execute user code, import packages, attach modules, or create aliases.
- Explicit `python` selections use an existing executable or venv instead of the ephemeral environment produced from the managed manifest.
- Explicit `python` selections do not mutate the managed requirements manifest.
- Requirements preparation preserves the session when possible and restarts according to `requirements.restart`.
- Restarting discards previous variables, imports, pending output, and active work.
- Replies state whether the session was unchanged or restarted and echo the full managed requirements manifest.
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
- Default `{}` preparation realizes the initial manifest and provides `numpy`.
- `requirements: {}` keeps the current manifest rather than clearing `numpy`.
- `action="add"` adds packages to the manifest; `action="remove"` removes exact package strings; `action="set"` replaces the manifest.
- `restart="no"` fails unchanged when satisfying the manifest would discard user state.
- `restart="yes"` restarts even when the current session already satisfies the manifest.
- Invalid XOR shapes, invalid enum values, relative Python paths, and unknown fields are rejected through the public tool surface.
- Absolute executable and venv paths are accepted.
- `python_version` values such as `3.11`, `3.11.4`, and `>=3.11,<3.13` are accepted and forwarded.
- Preparation can replace an active session and reports discarded work.
- Default requirements preserve the current Python session when the current runtime already provides the requested distribution metadata.
- Matching explicit Python executable preparation preserves the existing session.
- Explicit Python selection does not mutate the managed manifest.

## Decision Log

- 2026-06-21: Chose a separate `repl_prepare` tool rather than adding requirements to `repl`.
- 2026-06-21: Chose a Python-specific schema because each MCP server instance is specialized to one interpreter.
- 2026-06-21: Chose top-level `requirements` XOR `python`, with `python.executable` XOR `python.venv`.
- 2026-06-21: Chose `uv` as the only Python requirements resolver for this feature.
- 2026-06-21: Chose default Python packages `["numpy"]`.
- 2026-06-21: Chose to omit `timeout_ms` from `repl_prepare`.
- 2026-06-22: Changed `requirements` from exact per-call package selection to persistent manifest operations with `action` and `restart`.
