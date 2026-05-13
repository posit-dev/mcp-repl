# Embedded Python Backend

## Summary

- Replace the Python PTY subprocess backend with an embedded CPython runtime inside the worker process.
- Keep the public MCP tool surface, sandbox modes, oversized-output behavior, and image response shape unchanged.
- Preserve raw stdout/stderr capture for child processes and direct file-descriptor writes.

## Status

- State: completed
- Last updated: 2026-05-09
- Current phase: complete

## Current Direction

- Use one `mcp-repl` executable with server, R worker, and Python worker modes.
- Select the backend in worker mode from `MCP_REPL_INTERPRETER`; Python worker mode dynamically loads CPython from the selected Python executable's `sysconfig` metadata.
- Run CPython in process on the worker runtime thread with CPython's native
  interactive loop.
- Let CPython own worker stdin. The server writes raw request bytes to stdin and
  sends request-boundary metadata over sideband IPC.
- Install `PyOS_InputHook` for idle/readline sideband events. Python-level
  `input()`, `sys.stdin`, `sys.stdout`, and `sys.stderr` are remapped to objects
  backed by Rust callbacks so runtime-owned text moves over ordered IPC.
- Keep Python plot support in a small bootstrap script that installs matplotlib hooks and calls Rust-backed module functions for image events.

This keeps the same server/worker protocol shape as R without linking or loading CPython in R worker mode.

## Long-Term Direction

- Python and R should share the same worker/server boundary: stdin carries raw request bytes, IPC carries request boundaries and other sideband facts, embedded runtime callbacks own readline/output facts, and server completion should not depend on backend-specific raw prompt parsing.
- A later slice may narrow the Python-level stream shims if CPython grows a
  public output callback equivalent to the readline/input hooks.

## Phase Status

- Phase 0: completed - studied reticulate's CPython startup, output remap, and readline bridge.
- Phase 1: completed - embed CPython in the existing worker process.
- Phase 2: completed - broaden public regression coverage and update stale Python subprocess assumptions.
- Phase 3: completed - removed the helper binary and switched to dynamic CPython loading.
- Phase 4: completed - full required Rust checks pass.

## Locked Decisions

- Python runtime-owned stdout/stderr should use IPC `output_text`, not raw PTY capture.
- Raw stdout/stderr readers stay in place for child processes and direct fd writes.
- EOF/restart keeps the existing process-respawn model; CPython is not finalized and reinitialized in the same worker process.
- The initial implementation targets one embedded CPython session per worker process.
- `sys.executable` is populated from the same Python executable used to locate the dynamically loaded CPython library.
- The main `mcp-repl` binary must not link CPython, and R worker mode must not load CPython.
- Python worker interrupts call the dynamically loaded `PyErr_SetInterrupt` and clear queued input so buffered tail expressions do not run after interruption.

## Follow-Up Questions

- Whether arbitrary Python program selection must support installations whose `sysconfig` metadata does not expose a loadable CPython library.
- Whether Windows support should land in the same slice or follow once the Unix happy path is stable.

## Next Safe Slice

- Follow up only if a supported Python installation does not expose a loadable CPython library through `sysconfig`.

## Stop Conditions

- Stop and ask for a design decision if preserving arbitrary runtime Python executable selection conflicts with embedded CPython initialization.
- Stop and update this plan if the implementation needs a multi-PR split.

## Decision Log

- 2026-05-09: Use the existing worker process boundary for Python so Python and R share the same server architecture.
- 2026-05-09: Use CPython's native interactive loop instead of PTY prompt
  parsing so Python owns raw stdin while runtime-owned text can move over
  ordered IPC.
- 2026-05-09: Keep raw stdout/stderr capture only for subprocesses and direct file-descriptor writes.
- 2026-05-09: Briefly split Python into `mcp-repl-python-worker`; rejected that shape because the target architecture is one executable with server, R worker, and Python worker modes.
- 2026-05-09: Replaced the helper binary with one `mcp-repl` binary and a reticulate-style dynamic CPython loader.
- 2026-05-09: Installed `PyOS_InputHook` and dynamic `PyErr_SetInterrupt`;
  Python-level stream and input objects remain necessary because CPython does
  not expose a direct stdout/stderr callback pointer.
- 2026-05-09: Removed stdin framing; the server now writes raw request bytes to stdin and announces the byte count on IPC.
- 2026-05-09: Required Rust checks pass for the dynamic-loader Python worker.
