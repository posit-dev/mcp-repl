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

## Timeline vs completion

The important design split is not "files mode vs pager mode". It is:

- timeline resolution: reconstruct the visible output order from text plus
  sideband facts
- completion cleanup: once the server knows a request has finished, trim echoed
  input, append protocol warnings, and restore the final prompt

Timeline resolution must not depend on request completion. For example, the
server does not need to wait for completion to know that a `plot_image` event
belongs before a later `readline_result` echo. That ordering fact is already
present in the mixed timeline.

Completion matters only for reply cleanup choices that are unsafe while a
request is still in flight. In particular:

- timed-out or otherwise non-final drains must preserve echoed input so the user
  can still see what is running
- completed replies may trim or drop echo-only content once the server knows the
  request is settled

The intent is one true visible timeline per output surface, with completion used
only as a later presentation step.

Echo matching must be driven by the sideband facts themselves:

- `readline_start` supplies the prompt text the worker actually showed
- `readline_result` is emitted by the worker, but it describes the exact
  prompt text and input line that `readline` consumed and echoed
- the server should match and collapse those exact sideband facts
- the server should not parse visible output looking for prompt shapes such as
  `>`, `...`, or `Browse[n]>`

That matching is only opportunistic:

- the stdout/stderr pipes remain the authoritative visible text stream
- forked children, spawned subprocesses, or other writers may interleave with
  or corrupt what would otherwise have been a clean echoed line
- if exact sideband-to-stdout matching fails or becomes ambiguous, the server
  should degrade softly to raw captured stdout/stderr for that region, without
  eliding echo or inventing a cleaned-up transcript

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
