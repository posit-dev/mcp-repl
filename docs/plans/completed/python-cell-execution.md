# Python Cell Execution

## Summary

- Change built-in Python from CPython interactive-line request execution to
  persistent cell execution.
- Keep embedded CPython, the existing sideband worker protocol, managed stdin,
  and Python plot capture.
- Do not add a new MCP tool mode, Jupyter/IPython dependency, or separate Python
  worker process.

## Status

- State: complete
- Last updated: 2026-06-20
- Current phase: complete

## Current Direction

- Rust owns request state and routes each non-empty input batch by worker state:
  idle batches run as complete cells, waiting-input batches feed stdin, and
  running cells reject or preserve the current busy behavior.
- Python owns only Python-native semantics that are awkward through the public C
  API: final-expression display through `sys.displayhook` and matplotlib plot
  hooks.

## Phase Status

- Phase 0: complete - added public failing tests for cell execution,
  displayhook, input waits, debugger interaction, and docs contract changes.
- Phase 1: complete - added explicit Python execution state and pending-cell
  dispatch.
- Phase 2: complete - replaced ordinary interactive execution with cell
  execution.
- Phase 3: complete - updated docs and retired continuation-prompt tests that
  no longer describe the public contract.
- Phase 4: complete - ran required checks and moved this plan to completed.

## Locked Decisions

- Python `repl` defaults to cell execution with persistent globals.
- Final expression display is part of v1 and must honor `sys.displayhook`.
- Follow-up input is stdin only after Python has reported an input wait.
- Route each non-empty payload once, at the top of the call. If Python is waiting for stdin, the whole payload is stdin; otherwise the whole payload is one cell.
- Keep a small Python helper for AST cell execution and plot hooks; do not move
  protocol ownership into Python.

## Open Questions

- None for this slice.

## Next Safe Slice

- None. This plan is complete.

## Stop Conditions

- Stop and update this plan if the implementation needs a Python worker process,
  Jupyter/IPython, a new public tool mode, or server-side Python source parsing.
- Stop if managed stdin for `input()`, `help()`, `pdb`, `sys.stdin`, or raw fd
  shims cannot be preserved through cell execution.

## Decision Log

- 2026-06-20: Chose embedded cell execution because agent submissions are
  cell-shaped, while interactive stdin remains necessary for running programs
  that ask for input.
- 2026-06-20: Kept embedded CPython and implemented cell execution with a small
  AST helper that honors `sys.displayhook`; Rust owns request routing and
  managed stdin state.
- 2026-06-20: Dropped server-side code/stdin splitting for Python. Follow-up
  stdin is accepted only after the running cell reports an input wait.
