# Output Timeline

This document explains how `mcp-repl` reconstructs visible REPL output from the
worker's separate text pipes and sideband IPC stream.

## Why this exists

The worker emits different kinds of information on different channels:

- stdout/stderr bytes travel on the normal process pipes.
- Sideband IPC carries structural events such as `readline_start`,
  `readline_result`, `plot_image`, `request_end`, and `session_end`.

Those channels do not arrive at the server in one globally ordered stream.
The server therefore maintains its own output timeline and resolves it into the
user-visible order when building a reply.

## Current model

There are two server-side timeline implementations because oversized-output mode
changes what must stay buffered between tool calls.

### Files mode

- `src/pending_output_tape.rs` stores a mixed event tape for the current pending
  reply.
- Worker stdout/stderr bytes are buffered as `TextFragment` events.
- Sideband events are stored alongside text so later formatting can suppress
  echoed input and respect request boundaries.
- When a reply is sealed, `PendingOutputSnapshot::format_contents()` converts the
  tape into `WorkerContent`.

### Pager mode

- `src/output_capture.rs` stores text in the global output ring and stores image
  or server-status events at byte offsets within that ring.
- `src/worker_process.rs` reads ranges from that ring, collapses echoed input,
  and then asks `src/pager/` to page the resulting mixed text/image stream.

## Ownership split

- The worker is responsible for running the normal backend REPL and reporting the
  sideband facts it directly observes.
- The server is responsible for timeline reconstruction.
- The worker must not try to solve cross-channel ordering by pretending to know
  exactly when stdout bytes became visible to the server.

In practice, that means image-vs-stdout ordering fixes belong in server timeline
resolution, not in the wire protocol.

## What the timeline must preserve

- Worker text must remain in the order observed on its stdout/stderr pipes.
- Sideband `readline_result` events define the order in which input lines were
  consumed.
- Sideband `plot_image` events define when plot updates happened relative to
  other sideband events.
- Visible replies must preserve evaluation order even when text-pipe delivery and
  sideband delivery race.

The important consequence is that "arrival order at the server" is not always
the same thing as "execution order in the backend".

## Discovery map

- `src/output_capture.rs`: pager-mode output ring and event storage.
- `src/pending_output_tape.rs`: files-mode mixed event tape.
- `src/worker_process.rs`: request completion, echo suppression, and reply
  assembly.
- `src/ipc.rs`: sideband event intake and per-request IPC bookkeeping.
- `docs/worker_sideband_protocol.md`: wire-level IPC contract.

## Current limitation

This document describes the current server contract, not future simplification
work. Broader changes to worker dumbness, request completion inference, or R
worker structure should be tracked in `docs/futurework/`.
