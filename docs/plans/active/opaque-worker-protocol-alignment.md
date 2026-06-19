# Opaque Worker Protocol Alignment

## Status

- State: superseded
- Last updated: 2026-06-19
- Superseded by: `docs/worker_sideband_protocol.md`

## Outcome

The IPC-queued opaque worker protocol is now documented as the source-of-truth
worker/server contract. The earlier v4 design with `input_id` fields and
`readline_start` prompt advisory messages is superseded by protocol v5:

- accepted `repl` input is sent over IPC with `input_batch`,
- follow-up input after a runtime input wait starts a fresh `input_batch`,
- the worker owns the input queue and runtime placement,
- runtime stdin, PTY, `ReadConsole`, `PyOS_Readline`, `sys.stdin`, and direct
  fd bridge details are worker-internal,
- worker readiness for input and successful same-worker input-batch reply boundaries
  are reported by `input_wait`,
- `input_id`, `readline_start`, `idle`, `stdin_wait`, `turn_start`, and
  `turn_input` are not protocol messages,
- client-requested interrupts are forwarded as payload-free `interrupt`
  messages whenever a worker endpoint exists,
- `session_end` is terminal for any active input,
- reset and teardown request worker exit with the server-to-worker `shutdown`
  lifecycle message,
- raw stdout/stderr, prompt-shaped text, PTY state, and timing do not drive
  completion.

Keep future protocol changes in `docs/worker_sideband_protocol.md` first. Do
not revive this plan as the active contract.
