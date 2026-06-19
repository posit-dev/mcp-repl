# Worker Sideband Protocol

This document describes the sideband protocol between the server and a
worker process. The channel is a UTF-8 JSON-lines stream, one JSON object
per line, carried over an IPC pipe.

This document defines worker protocol version 5. The server rejects
unsupported protocol versions before sending user input. Built-in R,
built-in Python, and custom workers share the same public contract: workers
are opaque runtimes, accepted input is sent over IPC with `input_batch`, and
the worker owns how queued text reaches the runtime.

The protocol is shaped around one invariant:

> The server may send `input_batch` only after the worker has emitted
> `input_wait`. The worker emits `input_wait` only when the runtime is waiting
> on the managed input boundary and the worker is available for the next
> `input_batch`.

## Ownership

The server owns the MCP reply. It captures output, enforces timeouts, detects
worker exit or IPC loss, creates output bundles, and returns partial replies
when an input batch times out or the worker crashes.

The worker owns the runtime input boundary. It may drive the runtime through a
pipe, PTY, callback, embedded queue, or interpreter-specific API. Only the
worker may assert that the runtime is waiting for more input and that queued
input from the previous batch has been consumed.

Prompt text is display and cache data only. It never determines routing or
readiness. The server does not infer readiness from stdin writes, PTY state,
prompt-looking output, raw stdout/stderr, or timing.

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

## Readiness Model

An input batch is one accepted non-empty client payload. Empty polls only drain
output or report current state; they never create input batches.

The worker starts unavailable. After `worker_ready`, the server waits for the
first `input_wait`. That initial `input_wait` marks the worker available and
caches its prompt. Every later `input_wait` also marks the worker available
and refreshes the cached prompt.

When the server sends `input_batch`, the worker becomes unavailable. While
unavailable, the server must not send another non-empty payload to the same
worker. The worker queues the input and wakes the managed runtime input path.
When the runtime consumes all queued bytes or lines and asks the managed input
boundary for more input, the worker emits `input_wait`. If a request is
active, that `input_wait` completes it; if no request is active, it only
refreshes availability and prompt cache.

The worker emits `input_line` whenever it returns accounted runtime input from
the accepted batch. `input_line` is valid only while an input batch is in
flight.

`session_end` is terminal for any active input because the old runtime can no
longer consume follow-up input. Worker exit, sideband EOF, or process crash
without `session_end` is handled by the server as worker failure with captured
partial output.

Timeouts do not complete an input batch. On timeout, the server returns
captured partial output and keeps the worker unavailable. Later empty polls
continue draining output until the worker emits `input_wait`, emits
`session_end`, exits, or times out again.

## IPC Queue And Runtime Stdin

PTYs, ordinary stdin, language-level stdin wrappers, and runtime callbacks are
worker-internal transports. The server must not write managed request payloads
directly to the runtime's stdin as the steady-state execution path. Supported
runtime input travels through the worker's managed input path.

An IPC-queued worker keeps accepted input in worker-owned state until the
runtime consumes it:

1. Server sends `{ "type": "input_batch", "input": "x <- 1" }`.
2. Worker receives `input_batch`.
3. Worker normalizes input for its runtime, such as appending a final newline.
4. Worker enqueues bytes or lines in its active-input queue.
5. Runtime asks for input through a worker-owned boundary such as `ReadConsole`,
   `PyOS_ReadlineFunctionPointer`, `sys.stdin`, a direct fd read shim, a PTY,
   or another runtime hook.
6. Worker removes queued bytes or a queued line and returns it to that runtime
   boundary.
7. Worker emits `input_line` for the logical input delivered to the runtime.
8. Runtime asks the managed input boundary for more input with the queue empty.
9. Worker sends `{ "type": "input_wait", "prompt": ">" }`.
10. Server receives the readiness fact and finalizes any active MCP reply.

This model can use line or byte accounting inside the worker. That accounting
remains private because it is only evidence for whether the worker can
truthfully emit `input_wait`.

A PTY-backed worker must control buffering well enough for its checks to mean
what they claim. If the worker cannot know that queued or buffered active
input has been consumed, it must not emit `input_wait`. It should keep the
worker unavailable, emit `session_end`, or rely on server-side replacement
after timeout/reset.

Unsupported direct fd0 readers remain unsupported unless they are routed
through the managed worker input path. If a child process has sideband IPC
disabled, it must not be able to consume managed queued input by accident; it
should see EOF or whatever explicit worker policy applies.

## Direction: server -> worker

`input_batch`
- `{ "type": "input_batch", "input": <string> }`
- Sends one accepted non-empty client payload.
- The server may send this only while the worker is available from
  `input_wait`; sending it marks the worker unavailable.
- `input` is the decoded MCP `repl()` text. The worker owns appending a final
  newline when its runtime requires line-oriented input.

`interrupt`
- `{ "type": "interrupt" }`
- Sent whenever the client requests interrupt and a worker process or IPC
  endpoint exists.
- The server may also deliver the platform interrupt to the worker process or
  process group.
- This message is for worker-owned cleanup and state transition only. It does
  not carry user input and does not complete an input batch.
- After sending interrupt, the server marks the worker unavailable and waits
  for the next `input_wait` or `session_end`.
- If no worker exists, the server keeps the existing no-endpoint behavior and
  does not spawn a worker only to interrupt nothing.

`shutdown`
- `{ "type": "shutdown" }`
- Requests worker process shutdown during reset, replacement, or server
  teardown.
- This is lifecycle control, not input batch. It must not carry payload text or
  interpreter shutdown code.

Removed server-to-worker messages
- `turn_start`, `turn_input`, and identity-bearing `input_batch` are not part
  of protocol v5.

## Direction: worker -> server

Worker-to-server messages are strict: unknown fields, invalid enum values,
invalid base64, and unknown message types are protocol errors.

`worker_ready`
- `{ "type": "worker_ready", "protocol": { "name": "mcp-repl-worker", "version": 5 }, "worker": { "name": <string>, "version": <string> }, "capabilities": { "images": <bool> } }`
- Must be the first worker-to-server message for protocol workers.
- The server rejects unsupported protocol names or versions before sending
  user input.
- `worker.name` is diagnostic metadata. Server request handling must not
  branch on it.

`input_wait`
- `{ "type": "input_wait", "prompt": <string> }`
- Emitted when the runtime is waiting for more input through the managed input
  boundary and the worker is available for the next `input_batch`.
- If an input batch is active, this is the successful same-worker completion
  signal for that batch.
- If no input batch is active, this only refreshes availability and prompt
  cache.
- The prompt string is required; use an empty string if the runtime supplied no
  prompt.

`input_line`
- `{ "type": "input_line", "prompt": <string>, "text": <string> }`
- Records that the worker delivered a logical input line to the runtime at this
  point in the ordered sideband stream.
- `input_line` is valid only while an input batch is in flight.
- `prompt` and `text` are structural input facts, not runtime output.
  Submitted input must not be emitted as `output_text`.
- The final MCP reply elides synthetic input echoes by default. The server may reconstruct `prompt + text` from ordered `input_line` events for transcript finalization, echo trimming, debugging views, or other surfaces that need an interactive transcript.

Removed worker-to-server messages
- `readline_start`, `idle`, `stdin_wait`, and identity-bearing `input_wait`,
  `input_line`, or `session_end` are not part of protocol v5.

`output_text`
- `{ "type": "output_text", "stream": <"stdout"|"stderr">, "data_b64": <base64>, "is_continuation": <bool, optional> }`
- Carries worker-owned output bytes on the ordered sideband stream. The
  payload is base64 so workers can preserve bytes without depending on JSON
  string encoding.
- Prompt-looking bytes are ordinary output unless the worker reports them in
  `input_wait.prompt` or `input_line.prompt`.
- `is_continuation` marks bounded transport chunks that continue the same
  worker-owned output write. It defaults to `false`.
- Workers send output-critical frames synchronously: each JSON line is written,
  newline-terminated, and flushed before the send returns.
- Workers treat synchronous write failure as IPC failure. They must not
  silently fall back to stdout or stderr for output that is owned by the worker
  protocol.
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
- `{ "type": "session_end", "reason": <string>, "message_b64": <base64, optional> }`
- Indicates the worker session is terminating.
- `reason` is required for protocol workers. Recognized values are `shutdown`,
  `reset`, `runtime_exit`, `crash`, and `protocol_error`.
- This is terminal for the whole session, including any active input.
- After this event, the worker must not emit more output.

## Interrupt Recovery

Interrupt recovery is worker-owned and fail-closed. The server sends
`interrupt` whenever a client asks for interrupt and a worker endpoint exists.
The server then marks the worker unavailable and waits for `input_wait`,
`session_end`, process exit, or timeout.

An interrupt for a queued worker works like this:

1. Server sends `{ "type": "interrupt" }`.
2. Server may also deliver the platform interrupt to the worker process or
   process group.
3. Worker stops writing any not-yet-written queued input for the current batch.
4. Worker drops queued input that has not reached the runtime transport.
5. Worker attempts runtime-specific cleanup for input that may already be in
   the transport, such as draining readable PTY input, flushing terminal input,
   or asking the runtime to unwind.
6. Runtime either reaches a managed input wait, exits, or remains busy.
7. If the runtime reaches input wait and the worker can prove no active-batch
   input remains queued, in flight, buffered, or cleanup-uncertain, worker sends
   `{ "type": "input_wait", "prompt": <string> }`.
8. If the runtime exits, worker sends `session_end`.
9. If cleanup is uncertain, worker sends no `input_wait`. The worker remains
   unavailable until timeout, later recovery, `session_end`, or server
   replacement.

The worker may use runtime-specific cleanup mechanisms internally. Those
details are not protocol facts. If cleanup is only best-effort and old input
may still be buffered in a PTY, libc, readline, or interpreter state, the
worker must not emit `input_wait` for the interrupted input batch.

## Notes

- Raw stdout/stderr capture remains active for unowned output, such as child
  processes or direct file-descriptor writes. Raw capture must not drive
  completion, prompt detection, echo suppression, or interrupt routing.
- For PTY-backed workers, raw visible output may arrive from one terminal
  stream with terminal behavior such as CRLF translation, echo, terminal-width
  effects, and merged stdout/stderr identity. Worker-owned `output_text` frames
  preserve their declared stream; raw PTY output does not promise pipe-style
  stream fidelity.
- Prompt variants used for echo cleanup come from `input_wait.prompt` and
  `input_line.prompt`.
