# Worker Sideband Protocol (JSON Lines)

This document describes the sideband protocol between the server and a worker
process. The channel is a UTF-8 JSON-lines stream, one JSON object per line,
carried over an IPC pipe.

This document defines worker protocol version 3. The server rejects other
protocol versions before sending user input.

The protocol is shaped around one race-free invariant:

> The server does not infer that a worker is idle from stdin writes, PTY state,
> prompt-looking output, raw stdout/stderr, or timing. A same-worker turn is
> complete only when the worker emits `idle` for that turn, or when the worker
> session ends.

## Ownership

The server owns the MCP reply. It captures output, enforces timeouts, detects
worker exit or IPC loss, creates output bundles, and returns partial replies
when a turn times out or the worker crashes. A worker crash before `idle` is a
server-observed terminal condition for the current reply; it is not a protocol
race.

The worker owns the runtime input boundary. It may drive the runtime through a
pipe, PTY, callback, embedded queue, or interpreter-specific API, but only the
worker may assert that the runtime is waiting for more input and that no input
from the previous turn can satisfy that wait.

## Sideband Transport

- Unix: worker inherits two file descriptors through environment variables:
  - `MCP_REPL_IPC_READ_FD`
  - `MCP_REPL_IPC_WRITE_FD`
- Windows: worker connects to two server-created named pipes through
  environment variables:
  - `MCP_REPL_IPC_PIPE_TO_WORKER`
  - `MCP_REPL_IPC_PIPE_FROM_WORKER`
- Messages are serialized as UTF-8 JSON, one message per line.
- Worker-owned output and structural facts are ordered on the sideband stream.
  Raw stdout/stderr capture remains active only as unowned fallback output.

## Turn Model

The server allows at most one active turn per worker session. Each non-empty
`repl()` execution input becomes a sideband `turn_start` message with a fresh
`turn_id`. The input is a JSON string, so ordinary request payloads remain plain
UTF-8 text on the inspectable sideband stream. The worker owns newline
normalization and runtime placement.

Runtime stdin transport is a worker implementation detail. A worker may write
the turn text into an embedded queue, an ordinary pipe, a PTY master, or an
interpreter callback. The server must not use transport observations from any
of those mechanisms to decide request completion.

The worker emits exactly one terminal turn-boundary fact for a successful
same-session turn:

- `idle(turn_id)`: the runtime is waiting for new input, and no bytes or text
  from that turn can still satisfy that wait.

`session_end` is also terminal for any active turn because the old runtime can
no longer consume follow-up input. Worker exit, sideband EOF, or process crash
without `session_end` is handled by the server as worker failure with captured
partial output.

Timeouts do not complete a turn. On timeout, the server returns captured partial
output and keeps the turn active. Later empty polls continue draining output
until the worker emits `idle`, emits `session_end`, exits, or times out again.
The server must not send ordinary follow-up input to the same worker while the
turn remains active. It may send an interrupt for the active turn, reset or
replace the worker, or report that the worker is still busy.

## PTY And Stdin Workers

PTYs and ordinary stdin are worker-internal runtime transports. They may require
worker-internal accounting, but they must not expose that accounting as the
server's completion rule. The server sees only `turn_start`, output,
`idle`, `session_end`, and failure.

A PTY-backed worker must own the PTY master or an equivalent write endpoint. A
typical turn works like this:

1. Server sends `{ "type": "turn_start", "turn_id": 7, "input": "x <- 1" }`.
2. Worker receives `turn_start`.
3. Worker records active turn `7`.
4. Worker normalizes input for its runtime. For a line-oriented runtime, this
   usually means appending a final newline and splitting into worker-owned line
   items.
5. Worker enqueues those line items in an active-turn input queue.
6. Worker writer takes the next queued item and writes it to the PTY master.
7. PTY/kernel/runtime code may transform or buffer what was written. This is
   allowed, but it is no longer a server-visible accounting surface.
8. Runtime consumes input through the PTY.
9. Runtime reaches a worker-observed input wait, such as a readline callback,
   prompt hook, or equivalent interpreter event. Raw PTY output that looks like
   a prompt is not enough.
10. Worker receives that input-wait event.
11. Worker checks its active-turn state:
    - no queued line item remains for turn `7`;
    - no writer task has unwritten or in-flight input for turn `7`;
    - observable PTY input is empty, or the transport design gives an
      equivalent guarantee;
    - no interrupt or reset cleanup for turn `7` is pending or uncertain.
12. If all checks pass, worker sends
    `{ "type": "idle", "turn_id": 7, "prompt": ">" }`.
13. Server receives `idle(7)` and finalizes the turn reply from captured output.

If any check in step 11 fails, the worker does not emit `idle`. It lets the
runtime consume the pending input as part of turn `7`, writes the next queued
line item if needed, and repeats the check at the next worker-observed input
wait.

This model can use line accounting inside the worker. The worker may track
"line 1 was written", "line 2 is still queued", or "the current line write is
in flight." That line accounting remains private because it is only evidence
for whether the worker can truthfully emit `idle`. The server does not need to
know whether the worker proved `idle` with lines, bytes, callbacks, or a queue.

Server-visible line accounting is not sufficient for a PTY-backed runtime. A
line accepted by the PTY master is not necessarily a line consumed by the
runtime. The PTY may transform newlines, buffer pasted input, split writes,
merge multiple lines, flush input, or interact with runtime buffering. The
worker is the only component that can combine the transport facts with the
runtime input-wait event.

A PTY-backed worker must control buffering well enough for its checks to mean
what they claim. For example, if the runtime reads through a buffered `FILE*`,
the worker must either disable read-ahead buffering or place the input-wait
observation after those buffers are known not to contain active-turn input. If
the worker cannot make that guarantee, it cannot safely emit same-session
`idle` for that transport. It should use a queue or callback-backed runtime
boundary, emit `session_end`, or rely on server-side replacement after
timeout/reset.

## Direction: server -> worker

`turn_start`
- `{ "type": "turn_start", "turn_id": <integer>, "input": <string> }`
- Starts one runtime turn. The server must not send another `turn_start` for
  the same worker until the active turn reaches `idle`, reaches `session_end`,
  or the worker session is replaced.
- `input` is the decoded MCP `repl()` text. The worker owns appending a final
  newline when its runtime requires line-oriented input.

`interrupt`
- `{ "type": "interrupt", "turn_id": <integer> }`
- Sent when the server is about to interrupt the active turn. The server may
  also deliver the platform interrupt to the worker process or process group.
- This message is for worker-owned cleanup and state transition only. It does
  not carry user input and does not complete the turn.
- `turn_id` must match the worker's active turn. It is a stale-control guard,
  not a general request address. If it does not match, the worker must not apply
  the interrupt to any newer turn. It should ignore the stale control or end the
  session with a protocol error.
- After interrupt, the worker must emit `idle` only if it can prove no input
  from the interrupted turn can satisfy the next runtime input wait. If it
  cannot prove that, it must fail closed by staying active, ending the session,
  or relying on server reset/restart.

## Direction: worker -> server

Worker-to-server messages are strict: unknown fields, invalid enum values,
invalid base64, and unknown message types are protocol errors.

`worker_ready`
- `{ "type": "worker_ready", "protocol": { "name": "mcp-repl-worker", "version": 3 }, "worker": { "name": <string>, "version": <string> }, "capabilities": { "images": <bool> } }`
- Must be the first worker-to-server message for protocol workers.
- The server rejects unsupported protocol names or versions before sending user
  input.
- `worker.name` is diagnostic metadata. Server request handling must not branch
  on it.

`idle`
- `{ "type": "idle", "turn_id": <integer>, "prompt": <string> }`
- Emitted only after the runtime is waiting for more input and no input from
  `turn_id` can still satisfy that wait.
- This is the only successful same-worker turn completion signal.
- The prompt string is required; use an empty string if the runtime supplied no
  prompt.
- Prompt rendering is derived from this structured event, not from raw
  stdout/stderr parsing.

`input_line`
- `{ "type": "input_line", "turn_id": <integer>, "prompt": <string>, "text": <string> }`
- Records that the worker delivered a logical input line to the runtime at this
  point in the ordered sideband stream.
- `turn_id` must match the active turn.
- `prompt` and `text` are structural input facts, not runtime output.
  Submitted input must not be emitted as `output_text`.
- The final MCP reply elides synthetic input echoes by default. The server may reconstruct `prompt + text` from ordered `input_line` events for transcript finalization, echo trimming, debugging views, or other surfaces that need an interactive transcript.

`output_text`
- `{ "type": "output_text", "stream": <"stdout"|"stderr">, "data_b64": <base64>, "is_continuation": <bool, optional> }`
- Carries worker-owned output bytes on the ordered sideband stream. The payload
  is base64 so workers can preserve bytes without depending on JSON string
  encoding.
- Prompt-looking bytes are ordinary output unless the worker reports them in
  `idle.prompt`.
- `is_continuation` marks bounded transport chunks that continue the same
  worker-owned output write. It defaults to `false`.
- Workers send output-critical frames synchronously: each JSON line is written,
  newline-terminated, and flushed before the send returns.
- Workers treat synchronous write failure as IPC failure. They must not silently
  fall back to stdout or stderr for output that is owned by the worker protocol.
- Forked child processes with sideband IPC intentionally disabled are outside
  this worker-owned path and may continue to use inherited raw output streams.

`output_image`
- `{ "type": "output_image", "image_id": <string>, "mime_type": <string>, "data_b64": <base64>, "update": <bool> }`
- Carries worker-owned image bytes on the ordered sideband stream.
- `image_id` is worker-local source identity for update grouping. The server
  owns MCP response image IDs.

`plot_image`
- `{ "type": "plot_image", "mime_type": <string>, "data": <base64>, "is_update": <bool>, "source": <string|null> }`
- Built-in adapters use this event for plot output.
- There is no plot-image acknowledgement message.
- Workers must not delay stdout/stderr output waiting for sideband responses.

`session_end`
- `{ "type": "session_end", "reason": <string>, "message_b64": <base64, optional>, "turn_id": <integer, optional> }`
- Indicates the worker session is terminating.
- `reason` is required for protocol workers. Recognized values are `shutdown`,
  `reset`, `runtime_exit`, `crash`, and `protocol_error`.
- If `turn_id` is present and matches the active turn, it is terminal for that
  turn. If it is absent, it is terminal for the whole session, including any
  active turn.
- After this event, the worker must not emit more output.

## Interrupt Recovery

Interrupt recovery is worker-owned and fail-closed. The server may send an
interrupt while a turn is active and may return partial output if recovery times
out. The server must not send interrupt-tail input to the same worker until the
active turn reaches `idle` or `session_end`.

An interrupt for a PTY-backed worker works like this:

1. Server sends `{ "type": "interrupt", "turn_id": 7 }`.
2. Server may also deliver the platform interrupt to the worker process or
   process group.
3. Worker receives `interrupt(7)`.
4. Worker verifies that `7` is still the active turn. If not, the worker must
   not apply the interrupt to a newer turn.
5. Worker stops writing any not-yet-written queued input for turn `7`.
6. Worker drops any queued line items for turn `7` that have not reached the
   runtime transport.
7. Worker attempts runtime-specific cleanup for input that may already be in
   the transport, such as draining readable PTY input, flushing terminal input,
   or asking the runtime to unwind.
8. Runtime either reaches a worker-observed input wait, exits, or remains busy.
9. If the runtime reaches input wait and the worker can prove no input from
   turn `7` remains queued, in flight, buffered, or cleanup-uncertain, worker
   sends `{ "type": "idle", "turn_id": 7, "prompt": <string> }`.
10. If the runtime exits, worker sends `session_end`.
11. If cleanup is uncertain, worker sends neither `idle` nor same-worker tail
    input. The active turn remains active until timeout, later recovery,
    `session_end`, or server replacement.

The worker may use runtime-specific cleanup mechanisms internally. Those
details are not protocol facts. If cleanup is only best-effort and old input may
still be buffered in a PTY, libc, readline, or interpreter state, the worker
must not emit `idle` for the interrupted turn.

## Notes

- Raw stdout/stderr capture remains active for unowned output, such as child
  processes or direct file-descriptor writes. Raw capture must not drive
  completion, prompt detection, echo suppression, or interrupt routing.
- For PTY-backed workers, raw visible output may arrive from one terminal stream
  with terminal behavior such as CRLF translation, echo, terminal-width effects,
  and merged stdout/stderr identity. Worker-owned `output_text` frames preserve
  their declared stream; raw PTY output does not promise pipe-style stream
  fidelity.
- Control-only interrupts are server-owned routing decisions: if a worker
  process already exists, the server forwards the interrupt to it; if no worker
  exists, the server must not spawn one only to interrupt nothing.
