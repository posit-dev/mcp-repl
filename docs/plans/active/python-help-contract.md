# Python Help Contract

## Summary

- Keep the documented Python `repl` help contract in-band for `help(obj)`, `help("topic")`, `help()`, and `pydoc.help(...)`.
- This branch does not ship a dedicated Python-help runtime change. The immediate goal is to verify the current behavior directly with public regression coverage and only patch startup if that coverage reproduces an external-pager regression.
- Leave package availability, plot support, and standard interactive-Python multiline semantics unchanged.

## Status

- State: active
- Last updated: 2026-04-06
- Current phase: verification-first follow-up

## Current Direction

- Treat the current docs as the product contract: Python help should stay in-band and should not hand control to an external pager.
- Add direct public coverage for `help(len)`, `pydoc.help(len)`, and interactive `help()` roundtrips.
- If those tests reproduce a pager prompt or wedged session, patch worker startup with the smallest stdlib-only fix.

## Long-Term Direction

- The long-term contract is simple: documentation and inspection helpers in the Python backend are ordinary inline text output, not nested terminal UIs.
- The current slice should not grow into generic support for external pagers, `less`, or arbitrary terminal applications inside the worker PTY.
- Terminal-environment cleanup is separate follow-up tech debt if it still matters after direct coverage lands.

## Phase Status

- Phase 0: completed. Capture the intended public contract in the tool descriptions.
- Phase 1: active. Add direct public regression coverage for direct and interactive Python help flows.
- Phase 2: pending. If the tests reproduce an external-pager regression, patch Python startup with a minimal stdlib-only override before the first prompt.
- Phase 3: pending. Reassess whether any remaining `TERM` warning or worker-env cleanup deserves separate follow-up.

## Locked Decisions

- Do not change the public tool schema.
- Do not replace `builtins.help` with a custom renderer.
- Use stdlib objects in tests such as `len` or `str`; do not depend on pandas or other optional packages for help regression coverage.
- Leave `docs/tool-descriptions/repl_tool_python.md` and `docs/tool-descriptions/repl_tool_python_pager.md` aligned with the current contract unless the runtime behavior changes.
- If a runtime patch is needed, prefer a narrow startup-time `pydoc` override before the first prompt instead of a second output path or custom help renderer.
- Do not treat missing `matplotlib`, missing `sklearn`, or the requirement for a terminating blank line in Python compound statements as part of this bug.

## Open Questions

- Does the current branch still reproduce the original external-pager failure for direct `help()` / `pydoc.help()` calls, or was it already addressed indirectly by other runtime changes?
- No other design questions are open until that is answered with direct public coverage.

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
