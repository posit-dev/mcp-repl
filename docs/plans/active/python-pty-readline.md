# Python PTY Readline

## User Scenario

An MCP client exposes Python's own interactive REPL. Python owns primary prompts,
continuation prompts, `input()`, and `pdb`-style command loops through CPython's
interactive/readline machinery. The server does not parse Python code, infer
continuation state, or emulate Python stdin semantics.

## Summary

- Move the embedded Python worker toward PTY-backed C stdin/stdout so CPython
  takes the `PyOS_ReadlineFunctionPointer` path for supported interactive input.
- Keep sideband IPC separate from PTY traffic, with the server continuing to
  write normalized request bytes to worker stdin, consume sideband facts, capture
  visible output, and finalize replies generically.
- Preserve `readline_start`, `readline_input`, and `readline_discard`
  accounting where practical.
- Accept that PTY mode may change output fidelity, including stdout/stderr stream
  merging, CRLF translation, echo behavior, terminal width effects, and
  isatty-dependent Python behavior.
- Keep unsupported direct fd stdin behavior explicit instead of adding more
  Python-level hooks.

## Status

- State: active
- Last updated: 2026-05-15
- Current phase: phase 1 pending
- Driving epic: #189, "Move embedded Python to PTY-backed CPython readline"
- Last completed slice: #190, "Plan Python PTY readline redesign"

## Current Direction

- Treat PTY use as a launch-time worker transport decision, not a steady-state
  sideband protocol feature.
- Add an explicit pipe-vs-PTY launch abstraction before changing Python behavior.
- Prove PTY transport independently with sideband kept on a separate IPC channel.
- Run embedded Python with C stdin and C stdout attached to a PTY so CPython sees
  TTY streams and calls `PyOS_ReadlineFunctionPointer`.
- Make the readline callback the supported stdin accounting point for Python
  interactive input, `input()`, and debugger command loops.
- Remove or sharply reduce the broad Python stdin bridge after the readline path
  covers the supported public behavior.

This path is preferred because it lets CPython own Python syntax and interactive
control flow while keeping the server's request handling interpreter-neutral.

## Long-Term Direction

- Python and R should share the same server/worker boundary shape: stdin carries
  request bytes, sideband carries structural facts, and backend-owned callbacks
  report readline, output, plots, and session termination.
- Python should no longer depend on broad Python-level replacements for
  `builtins.input`, `sys.stdin`, `os.read`, `open(0)`, or similar direct stdin
  APIs as the primary integration mechanism.
- Direct fd stdin consumers are not first-class supported behavior unless a later
  public contract explicitly adds that support.
- Current pipe behavior may remain for R, fixtures, and non-PTY workers.

## Phase Status

- Phase 0: completed - create this plan and record the design boundary (#190).
- Phase 1: pending - add a launch-time worker stdin transport abstraction (#191).
- Phase 2: pending - prove PTY worker transport while keeping sideband separate
  (#192).
- Phase 3: pending - run embedded Python on PTY-backed C stdin/stdout and prove
  CPython takes the readline path (#193).
- Phase 4: pending - make `PyOS_ReadlineFunctionPointer` the Python stdin
  accounting point (#194).
- Phase 5: pending - harden interrupt and reset cleanup for PTY readline (#195).
- Phase 6: pending - remove obsolete Python stdin bridge and direct-fd shims
  (#196).
- Phase 7: pending - update current-state docs and snapshots for the final PTY
  contract (#197).

## Locked Decisions

- The server must not parse Python code, strip prompts, or infer continuation
  state from PTY output.
- Sideband IPC remains a separate channel from PTY input/output.
- PTY selection belongs in worker launch configuration, not in steady-state MCP
  request handling.
- PTY mode may merge stdout/stderr fidelity. Tests and docs should describe the
  public contract instead of preserving pipe-only stream identity assumptions.
- `PyOS_InputHook` alone is not enough for this redesign because it does not make
  CPython route actual line reads through the supported accounting point.
- Jupyter-style cell execution is rejected because the product surface is
  Python's interactive REPL, not notebook cell evaluation.
- Broad Python-level stdin interception is rejected as the long-term design.
  Keep only compatibility code that is justified by public behavior tests.

## Open Questions

- What is the smallest launch abstraction that keeps pipe workers unchanged while
  making PTY setup explicit and fail-fast?
- How should tests prove that CPython uses `PyOS_ReadlineFunctionPointer` without
  relying on private helper behavior?
- Which `readline_discard` facts can be accounted for exactly when bytes are
  queued in the terminal driver during interrupt or reset cleanup?
- Does PTY mode require bounded input-delivery coordination with open ordering
  work in #149?
- Does interrupt cleanup need a protocol acknowledgement decision related to
  #168, or can terminal flushing plus public stale-input tests cover the
  contract?
- What terminal size and echo settings should be fixed for deterministic tests?
- What, if any, Windows support belongs in this initiative?

## Next Safe Slice

- Work #191 next: add a narrow launch-time stdin transport model that preserves
  existing pipe behavior by default and keeps Python on the current path until
  PTY transport is proven.
- Public or launch-facing tests should prove the default pipe launch path remains
  unchanged before any Python PTY behavior changes.

## Drain Loop Note

- Work one ready child issue at a time.
- For behavior changes, add the public failing test first, confirm the failure,
  implement the narrow fix, then rerun focused and required Rust checks.
- Commit each completed slice separately, close only the completed issue, and let
  the next session pick the next ready child.
- If a child grows beyond one reviewable slice, split it before coding and record
  the dependency links.

## Stop Conditions

- Stop and ask before adding server-side Python parsing or prompt inference.
- Stop and ask before sending sideband facts over the PTY.
- Stop and ask before treating direct fd stdin as first-class supported behavior.
- Stop and update this plan if PTY setup cannot satisfy CPython's TTY
  assumptions on the supported Unix path.
- Stop and update this plan if a later slice needs to change the public reply
  shape or sideband event contract.

## Decision Log

- 2026-05-15: Started the active plan for epic #189 so later slices can follow a
  single PTY/readline design boundary.
- 2026-05-15: Chose PTY-backed C stdin/stdout as the route to CPython's readline
  path because `PyOS_InputHook` alone does not account for actual line reads.
- 2026-05-15: Kept sideband IPC separate from PTY traffic so readline facts,
  plots, session termination, and future output facts remain structured.
- 2026-05-15: Rejected Jupyter cell mode and broad Python-level stdin
  interception because the supported surface is Python's own interactive REPL.
