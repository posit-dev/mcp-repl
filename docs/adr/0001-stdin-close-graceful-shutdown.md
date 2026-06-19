# ADR 0001: Use stdin close for graceful worker shutdown

## Status

Superseded by `docs/worker_sideband_protocol.md` version 3

## Date

2026-05-19

## Context

The server previously allowed workers to advertise interpreter-specific
graceful shutdown text through `worker_ready.graceful_shutdown.stdin`, such as
R `quit("no")` or Python `exit()`. During reset or shutdown, the server could
write that text to worker stdin as the graceful request.

That design had three problems:

- It created a race between shutdown text sent on stdin and `session_end`
  notifications sent on sideband.
- It allowed user-level input operations, such as R `readline()` or Python
  `input()`, to consume the graceful shutdown request as ordinary user input.
- It made the lifecycle contract harder to explain, reason about, and
  communicate to a model or human because graceful shutdown used a mixture of
  stdin code, sideband notification, and OS escalation.

## Decision

This decision is no longer active.

Graceful worker shutdown is requested only by closing the worker stdin
transport. The server must not send interpreter shutdown code over stdin and
must not add or use a sideband shutdown command to deliver graceful shutdown.

Workers must no longer advertise `worker_ready.graceful_shutdown.stdin`.
Built-in workers do not advertise `quit("no")`, `exit()`, or equivalent
shutdown text.

On reset, respawn, or server shutdown, the server closes worker stdin and waits
for the worker to exit naturally within the existing shutdown timeout. If the
worker is busy, ignores EOF, or otherwise does not exit, the existing
SIGTERM/SIGKILL or platform-equivalent escalation remains the fallback.

Worker-to-server `session_end` remains a notification that the worker session is
ending. It is not a shutdown request from the server to the worker.

## Consequences

- There is no race between stdin shutdown code and sideband shutdown delivery.
- A blocked user prompt cannot receive interpreter shutdown text as an answer.
- Idle interpreters can still exit normally on EOF and run normal finalization
  where supported.
- Graceful finalization is best effort. User code can handle EOF and continue,
  or remain busy, in which case OS escalation terminates the worker.
- The server/worker lifecycle is simpler: stdin carries user input and EOF,
  sideband carries facts observed by the worker, and process control remains
  server-owned.

## Superseding Decision

Worker protocol version 3 moves accepted input onto IPC-managed turn queues.
Because managed request input no longer travels over worker stdin, stdin EOF is
not a reliable lifecycle signal for built-in workers. The server now requests
worker shutdown with the server-to-worker `shutdown` sideband message, then
falls back to stdin close and bounded process termination if the worker does not
exit promptly.

The replacement keeps the original race fix: the server still does not send
interpreter shutdown code as user-readable stdin. `shutdown` carries no
`turn_id`, text payload, or interpreter expression.
