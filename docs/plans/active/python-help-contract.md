# Python Help Contract

## Summary

- Repair the Python `repl` help contract so `help(obj)`, `help("topic")`, `help()`, and `pydoc.help(...)` stay in-band and never hand control to CPython's external pager.
- Keep the fix narrow: patch Python help behavior in the worker startup path and add public regression coverage.
- Leave package availability, plot support, and standard interactive-Python multiline semantics unchanged.

## Status

- State: active
- Last updated: 2026-03-23
- Current phase: planning

## Current Direction

- Fix the bug in the Python worker startup script by forcing `pydoc` to use its plain pager before the first user prompt.
- Use the existing prompt, `input()`, timeout, and interrupt machinery unchanged so help output follows the same request lifecycle as normal REPL output.
- Add stdlib-only public tests that prove Python help no longer prints pager prompts, wedges the worker busy, or requires an interrupt/reset to recover.

## Long-Term Direction

- The long-term contract is simple: documentation and inspection helpers in the Python backend are ordinary inline text output, not nested terminal UIs.
- The current slice should not grow into generic support for external pagers, `less`, or arbitrary terminal applications inside the worker PTY.
- Terminal-environment cleanup is a separate concern. If it proves worth fixing, handle it as worker-env hygiene in a later slice without reopening the help design.

## Phase Status

- Phase 0: completed. Reproduce the bug through the public tool surface and confirm the mismatch with the documented Python `repl` contract.
- Phase 1: pending. Patch Python worker startup so `pydoc` always renders plain in-band help.
- Phase 2: pending. Add public regression coverage for direct and interactive Python help flows.
- Phase 3: pending. Reassess whether the Python worker `TERM` warning merits a separate follow-up.

## Locked Decisions

- Do not change the public tool schema.
- Do not replace `builtins.help` with a custom renderer. Keep stdlib help behavior and remove only the external pager path.
- In `python/driver.py`, import `pydoc` during startup and set:
  - `pydoc.pager = pydoc.plainpager`
  - `pydoc.getpager = lambda: pydoc.plainpager`
- Apply the `pydoc` override before the first prompt is emitted so the first help call cannot cache the wrong pager behavior.
- Use stdlib objects in tests such as `len` or `str`; do not depend on pandas or other optional packages for help regression coverage.
- Leave `docs/tool-descriptions/repl_tool_python.md` unchanged unless the implementation forces a different contract. The goal is to make runtime behavior match the current docs again.
- Do not treat missing `matplotlib`, missing `sklearn`, or the requirement for a terminating blank line in Python compound statements as part of this bug.

## Open Questions

- None for the help-fix slice.
- The `TERM=xterm-ghostty` warning remains open only as possible follow-up tech debt, not as a blocker for the help fix.

## Next Safe Slice

- Patch `python/driver.py` to force `pydoc` plain-pager behavior at startup.
- Add a direct help regression test that runs `help(len)` and asserts:
  - output contains `Help on`
  - output does not contain `Press RETURN`
  - output does not contain `--More--`
  - output does not remain busy
- Add a second regression test for `pydoc.help(len)` with the same expectations.
- Add an interactive help roundtrip test:
  - call `help()`
  - wait for `help>`
  - request `len`
  - send `q`
  - run a normal Python command and assert the session is back at `>>>`

## Stop Conditions

- Stop if the fix requires a second output path outside the existing prompt/request-end model.
- Stop if the simplest `pydoc` pager override does not cover interactive `help()` and `pydoc.help(...)` together.
- Stop if the implementation starts depending on optional Python packages for regression coverage.
- Stop if the slice expands into generic nested pager or terminal-emulator support.

## Decision Log

- 2026-03-23: Scoped the active plan to Python help behavior only. Package availability and normal interactive-Python multiline semantics are out of scope.
- 2026-03-23: Chose the stdlib `pydoc` plain-pager override as the preferred implementation path instead of a custom help renderer.
- 2026-03-23: Deferred the Python worker terminal-type warning to separate tech debt so it does not block the help contract repair.
