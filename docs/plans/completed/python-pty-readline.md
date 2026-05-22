# Python PTY Readline

## User Scenario

An MCP client exposes Python's own interactive REPL. Python owns primary prompts,
continuation prompts, `input()`, and `pdb`-style command loops through CPython's
interactive/readline machinery. The server does not parse Python code, infer
continuation state, or emulate Python stdin semantics.

## Summary

- Move the embedded Python worker to PTY-backed C stdin/stdout on Unix and
  unsandboxed Windows so CPython takes the `PyOS_ReadlineFunctionPointer` path
  for supported interactive input.
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

- State: completed
- Last updated: 2026-05-20
- Current phase: complete
- Driving initiative: move embedded Python to PTY-backed CPython readline
- Final slice: current-state documentation and PTY output contract

## Current Direction

- Treat PTY use as a launch-time worker transport decision, not a steady-state
  sideband protocol feature.
- Keep the explicit pipe-vs-PTY launch abstraction.
- Keep PTY transport independent from sideband IPC.
- Run embedded Python with C stdin, stdout, and stderr attached to a PTY where
  the platform launch supports it so CPython sees TTY streams and calls
  `PyOS_ReadlineFunctionPointer`.
- Keep the PTY launch implementation platform-specific where sandbox launch
  semantics require it: Unix can allocate the PTY before sandbox exec, while
  Windows sandbox mode must attach ConPTY to the restricted child itself.
- Make the readline callback the supported stdin accounting point for Python
  interactive input, `input()`, and debugger command loops.
- Leave CPython's fd-backed stdin surface intact on the Unix PTY path instead of
  installing broad Python-level direct-fd stdin shims.

This path is preferred because it lets CPython own Python syntax and interactive
control flow while keeping the server's request handling interpreter-neutral.

## Diff Size Note

This branch originally looked like a large addition because it kept
transitional pipe-backed compatibility scaffolding while adding the PTY path.
Sandboxed Windows Python now creates ConPTY inside the restricted wrapper, so
the remaining broad stdin interception and protocol compatibility code should
be deleted instead of carried forward.

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

## Platform Launch Design

Unix and macOS/Linux sandboxed workers should allocate the PTY in the server
before the sandbox wrapper applies restrictions, then spawn the wrapper or
worker with the PTY slave as C stdin/stdout/stderr. Sideband file descriptors
must stay separate from the PTY and must survive exec. `portable-pty` can be
used for PTY allocation/master I/O where it fits, but its Unix
`SlavePty::spawn_command` path should not be used directly unless sideband file
descriptor preservation is handled, because that path closes extra file
descriptors before exec.

Windows sandboxed workers need a different shape. The outer server should still
spawn the Windows sandbox wrapper through ordinary pipes, but the wrapper should
create ConPTY after ACL/restricted-token setup and launch the restricted worker
directly into that ConPTY with `CreateProcessAsUserW` plus extended startup
attributes. The wrapper then forwards server stdin to the ConPTY input pipe and
ConPTY output to wrapper stdout. Sideband named pipes remain separate from
ConPTY traffic. PTY mode may merge stdout/stderr on Windows.

`portable-pty` is not the right Windows sandbox launch boundary as-is because
its ConPTY spawn path owns `CreateProcessW`; it does not apply this repo's
restricted token, launch ACL state, prepared capability SID, or job lifetime
setup. A local Windows ConPTY launch adapter, or an upstreamable extension that
allows the sandbox wrapper to provide the process creation step, is the intended
route.

## Phase Status

- Phase 0: completed - create this plan and record the design boundary.
- Phase 1: completed - add a launch-time worker stdin transport abstraction.
- Phase 2: completed - prove PTY worker transport while keeping sideband separate
  from visible PTY output.
- Phase 3: completed - run embedded Python on PTY-backed C stdin/stdout and prove
  CPython takes the readline path.
- Phase 4: completed - make `PyOS_ReadlineFunctionPointer` the Python stdin
  accounting point.
- Phase 5: completed - harden interrupt and reset cleanup for PTY readline.
- Phase 6: completed - remove obsolete Python stdin bridge and direct-fd shims
  from the Unix PTY path.
- Phase 7: completed - update current-state docs for the final Unix PTY
  contract and output tradeoffs.

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

## Remaining Follow-Up

- Sandboxed Windows Python still has a pipe-backed compatibility path. A future
  Windows wrapper ConPTY slice should attach ConPTY inside the restricted child
  launch boundary, then revisit whether the remaining Python-side stdin bridges
  can be removed.
- If future ordering work needs stricter input-delivery coordination, it should
  preserve the current boundary: sideband facts describe observed runtime events;
  the server must not parse Python prompts from visible PTY output.

## Completion Notes

- The current repository contract is documented in `docs/architecture.md`,
  `docs/worker_sideband_protocol.md`, `docs/output_timeline.md`, and
  `docs/testing.md`.
- Direct fd stdin consumers are not first-class request-completion behavior
  unless a later public contract adds that support.

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
  assumptions on the supported Unix or Windows sandbox paths.
- Stop and update this plan if a later slice needs to change the public reply
  shape or sideband event contract.

## Decision Log

- 2026-05-15: Started the active plan so later slices can follow a
  single PTY/readline design boundary.
- 2026-05-15: Chose PTY-backed C stdin/stdout as the route to CPython's readline
  path because `PyOS_InputHook` alone does not account for actual line reads.
- 2026-05-15: Kept sideband IPC separate from PTY traffic so readline facts,
  plots, session termination, and future output facts remain structured.
- 2026-05-15: Rejected Jupyter cell mode and broad Python-level stdin
  interception because the supported surface is Python's own interactive REPL.
- 2026-05-15: Chose a platform-specific PTY launch boundary for Windows
  sandbox support: ConPTY must be attached to the restricted child from inside
  the sandbox wrapper, not merely to the outer wrapper process.
- 2026-05-15: Added an explicit launch-time stdin transport model. Built-in R,
  built-in Python, and the current protocol fixture launch through pipe stdin.
- 2026-05-15: Proved Unix custom-worker PTY launch with sideband IPC preserved
  on separate inherited file descriptors. The fixture test covers PTY echo,
  CRLF output conversion, visible PTY output capture, and sideband prompt
  completion before switching Python to PTY.
- 2026-05-15: Switched built-in Unix Python launch to PTY-backed C
  stdin/stdout/stderr, disabled terminal echo for Python determinism, and kept
  the Python-level stdin bridge installed for compatibility while CPython owns
  `builtins.input`.
- 2026-05-15: Hardened built-in Unix Python interrupt cleanup by having the
  worker process its private interrupt cleanup message before the server sends
  SIGINT. The worker drains exact queued active-turn bytes and emits
  `readline_discard` before flushing terminal input for unknown leftovers.
  Bytes flushed from terminal state without being observed remain unreported.
- 2026-05-15: `repl_reset` while Python is blocked in readline now replaces the
  worker cleanly; stale prompt/input state from the old PTY-backed worker does
  not carry into the replacement session.
- 2026-05-15: Removed the Unix Python stdin bridge thread from the PTY runtime
  path and stopped installing Python-level direct-fd stdin shims there. The
  supported PTY path leaves CPython's `sys.stdin`, `open`, `os.read`,
  `os.readv`, and `io.FileIO` surfaces intact; request-completion accounting
  remains tied to `PyOS_ReadlineFunctionPointer`.
- 2026-05-20: Added unsandboxed Windows ConPTY launch for built-in Python,
  keeping sideband named pipes separate from PTY traffic and using
  sideband-aware direct-stdin bridges only on Windows so CRLF and console reads
  remain accountably tied to active MCP input.
