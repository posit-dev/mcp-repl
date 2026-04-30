# Python Help Contract

## Summary

- Keep the documented Python `repl` help contract in-band for `help(obj)`, `help("topic")`, `help()`, and `pydoc.help(...)`.
- The tool descriptions already document that contract.
- Direct public regression coverage now exists for native Python help flows, and startup pins `pydoc` to its plain in-band pager.

## Status

- State: completed
- Last updated: 2026-04-25
- Current phase: closed

## Current Direction

- Treat the current docs as the product contract: Python help should stay in-band and should not hand control to an external pager.
- Keep direct public coverage for `help(len)`, `pydoc.help(len)`, and interactive `help()` roundtrips against the native Python backend.
- Keep the startup-time `pydoc` plain-pager override so inherited `PAGER`, `MANPAGER`, or terminal settings cannot hand control to an external pager.

## Long-Term Direction

- The long-term contract is simple: documentation and inspection helpers in the Python backend are ordinary inline text output, not nested terminal UIs.
- This slice should not grow into generic support for external pagers, `less`, or arbitrary terminal applications inside the worker PTY.
- Terminal-environment cleanup is separate follow-up work if it still matters after direct coverage lands.

## Locked Decisions

- Do not change the public tool schema.
- Do not replace `builtins.help` with a custom renderer.
- Use stdlib objects in tests such as `len` or `str`; do not depend on pandas or other optional packages for help regression coverage.
- Leave `docs/tool-descriptions/repl_tool_python.md` and `docs/tool-descriptions/repl_tool_python_pager.md` aligned with the current contract unless runtime behavior changes.
- If a runtime patch is needed, prefer a narrow startup-time `pydoc` override before the first prompt instead of a second output path or custom help renderer.
- Do not treat missing `matplotlib` as fatal to tests, but do updated tests to bootstrap a python environment with the dependencies we need using uv.
- Do not treat reticulate coverage, optional package availability, or ordinary multiline Python semantics as part of this bug.

## Outcome

- The native Python backend keeps direct `help()` / `pydoc.help()` flows in-band under the public test harness, including environments with interactive pager variables.
- The plan closes with the narrow startup-time `pydoc` plain-pager override described in the locked decisions.

## Completed Slice

- Added direct regression coverage for `help(len)`, `pydoc.help(len)`, and an interactive `help()` roundtrip that asserts output stays inline, does not show `Press RETURN` or `--More--`, and does not leave the session busy.
- Added files-mode snapshots for the same public Python help flow.
- Patched `python/driver.py` to use `pydoc.plainpager` before the first prompt.

## Stop Conditions

- Stop if reproducing the bug requires a second output path outside the existing prompt/request-end model.
- Stop if the slice expands into generic nested pager or terminal-emulator support.
- Stop if coverage starts depending on optional Python packages instead of stdlib objects.

## Decision Log

- 2026-03-23: Scoped the follow-up to Python help behavior only. Package availability and normal interactive-Python multiline semantics are out of scope.
- 2026-03-23: Chose the stdlib `pydoc` plain-pager override as the preferred fallback if a runtime patch is still needed.
- 2026-03-23: Deferred worker terminal-type warnings to separate tech debt so they do not block the help contract.
- 2026-04-06: Reframed the slice as verification-first follow-up work because this branch keeps the in-band help contract in docs but does not land a dedicated Python-help runtime patch.
- 2026-04-16: Curated the plan after adjacent Windows and reticulate fixes landed elsewhere; the remaining gap is direct native Python help coverage.
- 2026-04-25: Landed direct public regression coverage for `help(len)`, `pydoc.help(len)`, and interactive `help()` roundtrips, plus the startup-time `pydoc.plainpager` override needed to keep inherited pager environments in-band.
