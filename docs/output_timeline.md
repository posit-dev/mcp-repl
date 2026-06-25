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

The server owns one canonical `OutputTimeline` in `src/output_capture.rs` for a
worker manager. The timeline stores raw stdout/stderr bytes, worker-owned
`output_text` IPC bytes, images, server status text, consumed-input sideband
events, request boundaries, input waits, and session-end markers. Text spans
record stream, origin, source, offsets, and byte ranges without mutating the
stored bytes.

`OutputRing` is the timeline's bounded storage backend, not a second assembly
path. It keeps timeline bytes and zero-byte events at offsets so cursors can
drain ranges without copying mode-specific private buffers.

`src/pending_output_tape.rs` remains only as a thin compatibility facade for the
files path. It owns no separate event enum and does no independent formatting.
Files mode drains the shared timeline through this facade so existing bundle and
settle-state call sites stay small.

Pager mode reads ranges from the same timeline and then applies pager policy:
page size, search, seen ranges, tail/head commands, elision markers, and footers.
It does not build a separate mixed output stream.

## Projection modes

Timeline storage is separate from presentation. A projection decides which
timeline facts become `WorkerContent` for a particular consumer:

- `ProjectionMode::Reply`: normal MCP replies. Generated input echoes are not
  shown.
- `ProjectionMode::Transcript`: full generated input echoes for transcript
  consumers.
- `ProjectionMode::Pager`: full generated input echoes, with only pager paging
  and elision allowed to reduce what is visible.
- `ProjectionMode::Bundle`: full generated input echoes in bundle transcripts,
  while reply-visible text remains governed by bundle preview policy.

Generated echoes are created only from consumed-input sideband events during
projection. The server never parses captured output to infer that text is an
echo, and it never suppresses captured output because it resembles input.

Stderr prefixing is also projection behavior. The timeline stores raw stderr
bytes and stream metadata; projection renders the `stderr: ` prefix while
preserving the stored bytes for range accounting and transcript ordering.

## Cursors

All cursors advance by offsets in the same timeline:

- the files reply cursor drains normal pending output
- the bundle transcript cursor writes worker-originated text into bundles
- the pager cursor tracks page/search/tail locations
- detached-prefix capture drains output produced before the next accepted
  request

Mode-specific code may decide how much projected content to expose, when to
spill, or how to page. It must not create a second source of truth for ordering.

## Timeline vs completion

The important design split is not "files mode vs pager mode". It is:

- timeline resolution: reconstruct the visible output order from text plus
  sideband facts
- completion presentation: once the server knows a request has finished, append
  protocol warnings and restore the final prompt

Timeline resolution must not depend on request completion. For example, the
server does not need to wait for completion to know that an `output_image` event
belongs before later worker-owned output. That ordering fact is already present
in the mixed timeline.

Completion matters only for reply presentation choices that are unsafe while a
request is still in flight. Timed-out, non-final, and completed drains all
preserve captured output bytes. Completion may add server-owned status,
warnings, or prompt text, but it must not inspect captured output to decide
whether text is an echo.

The intent is one true visible timeline per output surface, with completion used
only as a later presentation step.

Input sideband events are structural metadata. Echo generation, when a surface
needs it, is driven by those facts rather than by parsing captured output:

- `input_line` describes the exact prompt text and input line the worker
  delivered to the runtime
- `input_wait` supplies the prompt text for worker readiness and completed
  input batches
- the server may generate input echo text from `input_line` according to the
  projection mode
- the server may choose to skip echo generation for a presentation surface
- the server must not parse captured output looking for prompt shapes such as
  `>`, `...`, or `Browse[n]>`
- the server must not match sideband input against captured output in order to
  suppress or collapse text

Raw stdout/stderr remains authoritative for text that arrived on stdout/stderr,
even when it looks like a backend prompt or input echo. Forked children, spawned
subprocesses, PTY echo, runtime callbacks, and backend-owned `output_text`
frames can all produce prompt-shaped output; the timeline preserves those bytes.
Raw and sideband-owned output remain authoritative:

- raw stdout/stderr remains authoritative for text that arrived there, even when
  it looks like a backend prompt or input echo
- raw PTY output is authoritative for the bytes seen on the terminal stream,
  but it is not authoritative for separate stdout/stderr stream identity
- forked children, spawned subprocesses, runtime callbacks, backend-owned
  `output_text` frames, or other writers may interleave with runtime output
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
  delivered to the runtime and are the only source for generated input echoes.
- Sideband `output_image` events define when image updates happened relative to
  other sideband events.
- Visible replies must preserve evaluation order when that order is represented
  by sideband facts. They must not invent a strict order between unframed
  stdout/stderr bytes and sideband events that the server did not observe.

The important consequence is that "arrival order at the server" is not always
the same thing as "execution order in the backend".

## Discovery map

- `src/output_capture.rs`: canonical `OutputTimeline`, bounded storage, cursors,
  settle-state accounting, and projection drains.
- `src/resolved_output.rs`: conversion from timeline ranges into `WorkerContent`
  for each projection mode.
- `src/pending_output_tape.rs`: files-mode compatibility facade over
  `OutputTimeline`.
- `src/pager/`: pager policy over projected timeline ranges.
- `src/worker_process.rs`: request completion and reply assembly.
- `src/ipc.rs`: sideband event intake and per-request IPC bookkeeping.
- `docs/worker_sideband_protocol.md`: wire-level IPC contract.
