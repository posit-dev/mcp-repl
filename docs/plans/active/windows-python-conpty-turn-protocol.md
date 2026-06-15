# Windows Python ConPTY Turn Protocol

## Summary

- Add Windows Python PTY support around worker-owned turns.
- Keep server completion independent of stdin writes, prompt-looking output, PTY byte counts, and timing.
- Keep the existing Unix Python path unchanged for this slice unless shared protocol code needs small helpers.

## Status

- State: active
- Last updated: 2026-06-15
- Current phase: implementation

## Current Direction

- Route built-in Windows Python through worker protocol v3.
- Let `turn_start` carry accepted input to the Python worker.
- Let the Python worker own input queuing, logical `input_line` events, and `idle(turn_id)` emission.
- Use Windows ConPTY for the worker launch envelope and terminal-mode signaling, with sideband IPC remaining on named pipes.
- Feed CPython top-level input, `input()`, `sys.stdin`, and raw stdin bridges from the worker-owned turn queue at real input wait boundaries.

## Long-Term Direction

- Built-in Python should eventually use one worker-owned turn model on all platforms.
- Unix still has a legacy built-in adapter in this bounded slice; that is a compatibility tactic, not the target architecture.
- The Windows sandbox wrapper must create ConPTY for the restricted child when Python is launched through the wrapper. Placing only the wrapper process in a PTY is insufficient because the restricted child would still inherit wrapper-created pipes.

## Phase Status

- Phase 0: completed - read worker protocol, Python worker/session, launch, sandbox, and tests.
- Phase 1: completed - switch Windows built-in Python to v3 turn ownership.
- Phase 2: completed - add Windows ConPTY launch support for unsandboxed and sandboxed Python.
- Phase 3: active - expand Windows Python PTY/turn tests.
- Phase 4: pending - run required validation.

## Locked Decisions

- The server must not infer turn completion from stdin write success, prompt text, PTY output, byte counts, or timing.
- The Python worker may emit `idle(turn_id)` only after it observes a safe input wait and knows its active-turn input queue is empty.
- Interrupt cleanup fails closed: stale interrupts are ignored, and uncertain cleanup must not emit `idle`.
- Sideband IPC remains separate from ConPTY traffic.

## Open Questions

- Whether additional Windows-specific interrupt tests are needed beyond queued-tail cleanup coverage before marking the plan complete.
- Whether future work should replace the Python-level Windows ConPTY terminal identity shim with true CRT-visible ConPTY std handles if a stable launch recipe is found.

## Next Safe Slice

- Add or tighten interrupt and sandbox tests, then run the repository-required validation suite.

## Stop Conditions

- Stop and ask if proving input cleanup after Windows interrupt requires changing the user-visible interrupt contract.
- Stop and ask if sandboxed ConPTY launch requires broad restructuring of the Windows sandbox wrapper beyond a focused child stdio mode.

## Decision Log

- 2026-06-15: Use a checked-in living plan because the work crosses protocol, Python runtime, Windows process launch, sandbox wrapper behavior, and tests.
- 2026-06-15: Keep Unix Python behavior stable in the first slice to avoid mixing the Windows ConPTY rebuild with a cross-platform protocol migration.
- 2026-06-15: CPython on this Windows launch path did not invoke `PyOS_ReadlineFunctionPointer` because CRT `isatty()` stayed false, matching `Parser/myreadline.c`; Windows now uses a worker-owned queue REPL loop and Python stdin bridge instead of relying on ConPTY self-feed.
- 2026-06-15: The ConPTY wrapper keeps sideband IPC separate and tags the child with `MCP_REPL_WINDOWS_CONPTY=1`; embedded Python uses that tag to report terminal identity for managed fd 0/1/2 surfaces while reads still flow through turn state.
