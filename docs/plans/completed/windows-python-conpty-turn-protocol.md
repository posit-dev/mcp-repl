# Windows Python ConPTY Turn Protocol

## Summary

- Add Windows Python PTY support around worker-owned turns.
- Keep server completion independent of stdin writes, prompt-looking output, PTY byte counts, and timing.
- Keep the existing Unix Python path unchanged for this slice unless shared protocol code needs small helpers.

## Status

- State: completed
- Last updated: 2026-06-15
- Current phase: validation complete

## Final Direction

- Route built-in Windows Python through worker protocol v3.
- Let `turn_start` carry accepted input to the Python worker.
- Let the Python worker own input queuing, logical `input_line` events, and `idle(turn_id)` emission.
- Use Windows ConPTY for the worker launch envelope and terminal-mode signaling, with sideband IPC remaining on named pipes.
- Feed CPython top-level input, `input()`, `sys.stdin`, and raw stdin bridges from the worker-owned turn queue at real input wait boundaries.
- Rebind the embedded worker CRT fd `0/1/2` to `CONIN$` / `CONOUT$` before Python initializes so CPython's own TTY checks can invoke `PyOS_ReadlineFunctionPointer`.

## Long-Term Direction

- Built-in Python should eventually use one worker-owned turn model on all platforms.
- Unix still has a legacy built-in adapter in this bounded slice; that is a compatibility tactic, not the target architecture.
- The Windows sandbox wrapper must create ConPTY for the restricted child when Python is launched through the wrapper. Placing only the wrapper process in a PTY is insufficient because the restricted child would still inherit wrapper-created pipes.

## Phase Status

- Phase 0: completed - read worker protocol, Python worker/session, launch, sandbox, and tests.
- Phase 1: completed - switch Windows built-in Python to v3 turn ownership.
- Phase 2: completed - add Windows ConPTY launch support for unsandboxed and sandboxed Python.
- Phase 3: completed - expanded Windows Python PTY/turn tests.
- Phase 4: completed - ran required validation and CI.

## Locked Decisions

- The server must not infer turn completion from stdin write success, prompt text, PTY output, byte counts, or timing.
- The Python worker may emit `idle(turn_id)` only after it observes a safe input wait and knows its active-turn input queue is empty.
- Interrupt cleanup fails closed: stale interrupts are ignored, and uncertain cleanup must not emit `idle`.
- Sideband IPC remains separate from ConPTY traffic.

## Stop Conditions

- Stop and ask if proving input cleanup after Windows interrupt requires changing the user-visible interrupt contract.
- Stop and ask if sandboxed ConPTY launch requires broad restructuring of the Windows sandbox wrapper beyond a focused child stdio mode.

## Decision Log

- 2026-06-15: Use a checked-in living plan because the work crosses protocol, Python runtime, Windows process launch, sandbox wrapper behavior, and tests.
- 2026-06-15: Keep Unix Python behavior stable in the first slice to avoid mixing the Windows ConPTY rebuild with a cross-platform protocol migration.
- 2026-06-15: CPython on the initial Windows launch path did not invoke `PyOS_ReadlineFunctionPointer` because CRT `isatty()` stayed false, matching `Parser/myreadline.c`; the final design rebinds the embedded worker CRT fds to ConPTY console devices before Python initializes.
- 2026-06-15: The ConPTY wrapper keeps sideband IPC separate and tags the child with `MCP_REPL_WINDOWS_CONPTY=1`; embedded Python no longer fakes `os.isatty()` and uses the tag only to track managed raw stdin fds.
- 2026-06-15: Windows now uses CPython's native `PyRun_InteractiveOneFlags` loop and the worker-owned turn queue feeds `PyOS_ReadlineFunctionPointer`, `input()`, `sys.stdin`, and raw fd reads.
