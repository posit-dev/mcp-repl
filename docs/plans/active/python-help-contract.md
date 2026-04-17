# Python Help Contract

## Summary

- Keep the documented Python `repl` help contract in-band for `help(obj)`, `help("topic")`, `help()`, and `pydoc.help(...)`.
- The tool descriptions already document that contract.
- The remaining work is direct public regression coverage for native Python help flows; only patch startup if those tests fail.

## Status

- State: active
- Last updated: 2026-04-16
- Current phase: verification

## Current Direction

- Treat the current docs as the product contract: Python help should stay in-band and should not hand control to an external pager.
- Add direct public coverage for `help(len)`, `pydoc.help(len)`, and interactive `help()` roundtrips against the native Python backend.
- Keep runtime startup unchanged unless those tests reproduce a pager prompt or wedged session.

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

## Open Questions

- Does the native Python backend still reproduce any external-pager or stuck-session behavior for direct `help()` / `pydoc.help()` flows?
- If those direct tests pass without changes, should this plan close immediately with no runtime patch?

## Next Safe Slice

- Add a direct regression test for `help(len)` that asserts output stays inline, does not show `Press RETURN` or `--More--`, and does not leave the session busy.
- Add a second regression test for `pydoc.help(len)` with the same expectations.
- Add an interactive `help()` roundtrip test that requests `len`, exits help, and proves the session returns to `>>>`.
- Only if those tests fail, patch `python/driver.py` with the minimal stdlib override and keep the docs unchanged.

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
