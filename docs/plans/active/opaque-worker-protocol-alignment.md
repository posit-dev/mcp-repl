# Opaque Worker Protocol Alignment

## Status

- State: superseded
- Last updated: 2026-06-19
- Superseded by: `docs/worker_sideband_protocol.md`

## Outcome

The IPC-queued opaque worker protocol is now documented as the source-of-truth
worker/server contract:

- accepted `repl` input is sent over IPC with `turn_start`,
- same-turn stdin continuation input is represented by `turn_input`,
- the worker owns the input queue and runtime placement,
- runtime stdin, PTY, `ReadConsole`, `PyOS_Readline`, `sys.stdin`, and direct
  fd bridge details are worker-internal,
- successful same-session reply boundaries are reported by `idle` or
  `stdin_wait`,
- `session_end` is terminal for any active turn,
- reset and teardown request worker exit with the server-to-worker `shutdown`
  lifecycle message,
- raw stdout/stderr, prompt-shaped text, PTY state, and timing do not drive
  completion.

Keep future protocol changes in `docs/worker_sideband_protocol.md` first. Do
not revive this plan as the active contract.
