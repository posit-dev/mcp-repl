# Output Timeline

This document explains how `mcp-repl` reconstructs visible REPL output from the
worker IPC stream plus any raw text that still arrives on stdout or stderr.

## Why this exists

The worker emits different kinds of information on different channels:

- Worker-owned text may travel as ordered `output_text` IPC frames.
- Raw stdout/stderr bytes still capture unowned output from child processes,
  direct file-descriptor writes, or runtime/native code that bypasses the
  worker-owned output callbacks.
- PTY-backed workers expose raw terminal output through the PTY master, which
  may merge stdout/stderr identity and apply terminal behavior such as CRLF
  translation, echo, and width-dependent formatting.
- Sideband IPC carries structural events such as `input_line`, `input_wait`,
  `output_image`, and `session_end`.

Raw pipes and IPC do not arrive at the server in one globally ordered stream.
The server therefore maintains its own output timeline and resolves it into the
user-visible order when building a reply. Worker-owned `output_text` frames do
share IPC order with structural sideband facts.

## Current model

There are two server-side timeline implementations because oversized-output mode
changes what must stay buffered between tool calls.

### Files mode

- `src/pending_output_tape.rs` stores a mixed event tape for the current pending
  reply.
- Worker-owned `output_text` frames and raw stdout/stderr bytes are buffered as
  `TextFragment` events.
- Sideband events are stored alongside text so later formatting can respect
  request boundaries and reconstruct interactive transcripts when needed.
- When a reply is sealed, `PendingOutputSnapshot::format_contents()` converts the
  tape into `WorkerContent`.

### Pager mode

- `src/output_capture.rs` stores text in the global output ring and stores image
  or server-status events at byte offsets within that ring.
- `src/worker_process.rs` reads ranges from that ring and then asks
  `src/pager/` to page the resulting mixed text/image stream.

## Timeline vs completion

The important design split is not "files mode vs pager mode". It is:

- timeline resolution: reconstruct the visible output order from text plus
  sideband facts
- completion cleanup: once the server knows a request has finished, append
  protocol warnings and restore the final prompt

Timeline resolution must not depend on request completion. For example, the
server does not need to wait for completion to know that an `output_image` event
belongs before later worker-owned output. That ordering fact is already present
in the mixed timeline.

Completion matters only for reply cleanup choices that are unsafe while a
request is still in flight. In particular, timed-out or otherwise non-final
drains must still preserve runtime output so the user can see what is running.

The intent is one true visible timeline per output surface, with completion used
only as a later presentation step.

Input sideband events are structural metadata:

- `input_line` describes the exact prompt text and input line the worker
  delivered to the runtime
- `input_wait` supplies the prompt text for worker readiness and completed
  input batches
- the server should not render submitted input merely because it received
  `input_line`
- the server should not parse visible output looking for prompt shapes such as
  `>`, `...`, or `Browse[n]>`

Raw and sideband-owned output remain authoritative:

- raw stdout/stderr remains authoritative for text that did not arrive through
  `output_text`
- raw PTY output is authoritative for the bytes seen on the terminal stream,
  but it is not authoritative for separate stdout/stderr stream identity
- forked children, spawned subprocesses, or other writers may interleave with
  runtime output
- if output text resembles submitted input, the server still preserves it; output
  is output

## Ownership split

- The worker is responsible for running the normal backend REPL and reporting the
  sideband facts it directly observes.
- The server is responsible for timeline reconstruction.
- The worker must not try to solve raw pipe cross-channel ordering by pretending
  to know exactly when stdout bytes became visible to the server.
- The worker also must not delay raw stdout/stderr on sideband responses.
  Sideband IPC reports facts; it is not backpressure for raw visible text
  streams.

In practice, that means image-vs-stdout ordering fixes belong in server timeline
resolution, not in the wire protocol.

## What the timeline must preserve

- Worker text must remain in the order observed on its stdout/stderr pipes.
- For PTY-backed workers, worker text from the PTY master must remain in the
  order observed on that terminal stream.
- Sideband `input_line` events define the order in which logical input was
  delivered to the runtime.
- Sideband `output_image` events define when image updates happened relative to
  other sideband events.
- Visible replies must preserve evaluation order when that order is represented
  by sideband facts. They must not invent a strict order between unframed
  stdout/stderr bytes and sideband events that the server did not observe.

The important consequence is that "arrival order at the server" is not always
the same thing as "execution order in the backend".

## Discovery map

- `src/output_capture.rs`: pager-mode output ring and event storage.
- `src/pending_output_tape.rs`: files-mode mixed event tape.
- `src/worker_process.rs`: request completion and reply assembly.
- `src/ipc.rs`: sideband event intake and per-request IPC bookkeeping.
- `docs/worker_sideband_protocol.md`: wire-level IPC contract.

## Current limitation

This document describes the current server contract, not future simplification
work. Broader changes to output storage, response presentation, or worker
structure should be tracked in `docs/futurework/`.
