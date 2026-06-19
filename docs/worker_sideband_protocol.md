# Worker Sideband Protocol

This document describes the sideband protocol between the server and a
worker process. The channel is a UTF-8 JSON-lines stream, one JSON
object per line, carried over an IPC pipe.

This document defines worker protocol version 4. The server rejects
unsupported protocol versions before sending user input. Built-in R,
built-in Python, and custom workers share the same public input-batch
contract: accepted input is sent over IPC with `input_batch`, the worker
queues it, and the worker owns how queued text reaches the runtime.

The protocol is shaped around one invariant:

> The server does not infer that a worker is waiting from stdin writes,
> PTY state, prompt-looking output, raw stdout/stderr, or timing. A
> same-worker input batch is complete only when the worker emits
> `input_wait` for that `input_id`, or when the worker session ends.

## Ownership

The server owns the MCP reply. It captures output, enforces timeouts,
detects worker exit or IPC loss, creates output bundles, and returns
partial replies when an input batch times out or the worker crashes. A
worker crash before `input_wait` or `session_end` is a server-observed
terminal condition for the current reply; it is not a protocol race.

The worker owns the runtime input boundary. It may drive the runtime
through a pipe, PTY, callback, embedded queue, or interpreter-specific
API, but only the worker may assert that the runtime is waiting for more
input and that no input from the previous input batch can satisfy that
wait.

## Sideband Transport

- Unix: worker inherits two file descriptors through environment
  variables:
  - `MCP_REPL_IPC_READ_FD`
  - `MCP_REPL_IPC_WRITE_FD`
- Windows: worker connects to two server-created named pipes through
  environment variables:
  - `MCP_REPL_IPC_PIPE_TO_WORKER`
  - `MCP_REPL_IPC_PIPE_FROM_WORKER`
- Messages are serialized as UTF-8 JSON, one message per line.
- Worker-owned output and structural facts are ordered on the sideband
  stream.
  Raw stdout/stderr capture remains active only as unowned fallback output.

## Opaque Input-Batch Model

An input batch is one accepted non-empty client payload. The server
sends every accepted non-empty payload as a fresh `input_batch` with a
fresh `input_id`. The server does not decide whether that payload is
top-level REPL code or a response to `readline()`, `input()`, `help()`,
`sys.stdin`, or another runtime read. The runtime decides that naturally
because it is the component calling the worker's managed input callback.

The worker emits `input_wait` when all queued bytes or lines for the
active `input_id` have been consumed by the runtime and the runtime asks
the managed input boundary for more input. `input_wait` closes the input
batch. The next non-empty client payload starts a new `input_batch`; if
the runtime is blocked inside a managed input callback, that callback
returns the new batch's queued input to the runtime.

`session_end` is terminal for any active input because the old runtime
can no longer consume follow-up input. Worker exit, sideband EOF, or
process crash without `session_end` is handled by the server as worker
failure with captured partial output.

Timeouts do not complete an input batch. On timeout, the server returns
captured partial output and keeps the input batch active. Later empty
polls continue draining output until the worker emits `input_wait`,
emits `session_end`, exits, or times out again. The server must not send
another non-empty input to the same worker while the input batch remains
active. The server may send an interrupt for the active input, reset or
replace the worker, or report that the worker is still busy.

## IPC Queue And Runtime Stdin

PTYs, ordinary stdin, language-level stdin wrappers, and runtime
callbacks are worker-internal transports. They may require
worker-internal accounting, but they must not expose that accounting as
the server's completion rule. The server sees `input_batch`, output,
structural facts, `input_wait`, `session_end`, and failure.

An IPC-queued worker must keep accepted input in worker-owned state
until the runtime consumes it. The server must not write managed request
payloads directly to the runtime's stdin as its steady-state execution
path. If a child process has sideband IPC disabled, it must not be able
to consume managed queued input by accident; it should see EOF or
whatever explicit worker policy applies.

A typical queued input batch works like this:

1. Server sends
   `{ "type": "input_batch", "input_id": 7, "input": "x <- 1" }`.
2. Worker receives `input_batch`.
3. Worker records active input `7`.
4. Worker normalizes input for its runtime. For a line-oriented runtime,
   this usually means appending a final newline and splitting into queue
   items.
5. Worker enqueues those line items in an active-input input queue.
6. Runtime asks for input through a worker-owned boundary such as
   `ReadConsole`, `PyOS_ReadlineFunctionPointer`, `sys.stdin`, a direct
   fd read shim, a PTY, or another runtime hook.
7. Worker removes the next queued bytes or line from the queue and
   returns it to that runtime boundary.
8. Worker emits `input_line` for the logical input delivered to the
   runtime.
9. Runtime reaches a worker-observed input wait, such as a readline
   callback, prompt hook, or equivalent interpreter event. Raw PTY
   output that looks like a prompt is not enough.
10. Worker verifies that no queued, in-flight, buffered, or
    cleanup-uncertain input remains for `input_id` `7`.
11. If the check passes, worker sends
    `{ "type": "input_wait", "input_id": 7, "prompt": ">" }`.
12. Server receives the reply-boundary fact and finalizes the MCP reply
    from captured output.

If the check fails, the worker does not emit `input_wait`. It lets the
runtime consume pending input as part of input batch `7`, writes the
next queued item if needed, and repeats the check at the next
worker-observed input wait.

This model can use line or byte accounting inside the worker. The worker
may track "line 1 was delivered", "line 2 is still queued", or "a direct
stdin read has consumed N bytes." That accounting remains private
because it is only evidence for whether the worker can truthfully emit
`input_wait`. The server does not need to know whether the worker proved
completion with lines, bytes, callbacks, a queue, or a PTY.

A PTY-backed worker must control buffering well enough for its checks to
mean what they claim. For example, if the runtime reads through a
buffered `FILE*`, the worker must either disable read-ahead buffering or
place the input-wait observation after those buffers are known not to
contain active input-batch bytes. If the worker cannot make that
guarantee, it cannot safely emit same-worker `input_wait` for that
transport. It should use a queue or callback-backed runtime boundary,
emit `session_end`, or rely on server-side replacement after
timeout/reset.

## Direction: server -> worker

`input_batch`
- `{ "type": "input_batch", "input_id": <integer>, "input": <string> }`
- Starts one worker-owned input batch. The server must not send another
  `input_batch` for the same worker until the active input reaches
  `input_wait`, reaches `session_end`, or the worker session is
  replaced.
- `input` is the decoded MCP `repl()` text. The worker owns appending a
  final newline when its runtime requires line-oriented input.

`interrupt`
- `{ "type": "interrupt", "input_id": <integer, optional> }`
- Active-input form: `{ "type": "interrupt", "input_id": <integer> }`
- Sent when the server is about to interrupt the active input. The
  server may also deliver the platform interrupt to the worker process
  or process group.
- This message is for worker-owned cleanup and state transition only. It
  does not carry user input and does not complete the input batch.
- For an active queued input batch, `input_id` must match the worker's
  active input. It is a stale-control guard, not a general request
  address. If it does not match, the worker must not apply the interrupt
  to any newer input batch. It should ignore the stale control or end
  the session with a protocol error.
- A missing `input_id` is process-level control when the server has no
  active input id at the routing point.
- After interrupt, the worker must emit `input_wait` only if it can
  prove no input from the interrupted input batch can satisfy the next
  runtime input wait. If it cannot prove that, it must fail closed by
  staying active, ending the session, or relying on server
  reset/restart.

`shutdown`
- `{ "type": "shutdown" }`
- Requests worker process shutdown during reset, replacement, or server
  teardown.
- This is lifecycle control, not input batch. It must not carry
  `input_id`, payload text, or interpreter shutdown code.
- The worker should exit promptly. If it does not, the server keeps
  using its bounded process-control fallback.

Removed server-to-worker messages
- `turn_start` is not part of protocol v4. The input-carrying message is
  `input_batch`.
- `turn_input` is not part of protocol v4. Follow-up client input after
  a runtime input wait is sent as a new `input_batch`.

## Direction: worker -> server

Worker-to-server messages are strict: unknown fields, invalid enum
values, invalid base64, and unknown message types are protocol errors.

`worker_ready`
- `{ "type": "worker_ready", "protocol": { "name": "mcp-repl-worker", "version": 4 }, "worker": { "name": <string>, "version": <string> }, "capabilities": { "images": <bool> } }`
- Must be the first worker-to-server message for protocol workers.
- The server rejects unsupported protocol names or versions before
  sending user input.
- `worker.name` is diagnostic metadata. Server request handling must not
  branch on it.

`input_wait`
- `{ "type": "input_wait", "input_id": <integer>, "prompt": <string> }`
- Emitted only after the runtime is waiting for more input through the
  managed input boundary and no input from `input_id` can still satisfy
  that wait.
- This is the successful same-worker input-batch completion signal.
- The prompt string is required; use an empty string if the runtime
  supplied no prompt.
- Prompt rendering is derived from this structured event, not from raw
  stdout/stderr parsing.

`input_line`
- `{ "type": "input_line", "input_id": <integer>, "prompt": <string>, "text": <string> }`
- Records that the worker delivered a logical input line to the runtime
  at this point in the ordered sideband stream.
- `input_id` must match the active input.
- `prompt` and `text` are structural input facts, not runtime output.
  Submitted input must not be emitted as `output_text`.
- The final MCP reply elides synthetic input echoes by default. The server may reconstruct `prompt + text` from ordered `input_line` events for transcript finalization, echo trimming, debugging views, or other surfaces that need an interactive transcript.

`readline_start`
- `{ "type": "readline_start", "prompt": <string> }`
- Reports that the worker observed a runtime prompt before it knows
  whether the prompt will consume queued input or become `input_wait`.
- This is advisory prompt metadata only. It never completes an input
  batch and it does not carry input ownership.

Removed worker-to-server messages
- `idle` and `stdin_wait` are not part of protocol v4. Both states are
  reported as `input_wait`.

`output_text`
- `{ "type": "output_text", "stream": <"stdout"|"stderr">, "data_b64": <base64>, "is_continuation": <bool, optional> }`
- Carries worker-owned output bytes on the ordered sideband stream. The
  payload is base64 so workers can preserve bytes without depending on
  JSON string encoding.
- Prompt-looking bytes are ordinary output unless the worker reports
  them in `input_wait.prompt`.
- `is_continuation` marks bounded transport chunks that continue the
  same worker-owned output write. It defaults to `false`.
- Workers send output-critical frames synchronously: each JSON line is
  written, newline-terminated, and flushed before the send returns.
- Workers treat synchronous write failure as IPC failure. They must not
  silently fall back to stdout or stderr for output that is owned by the
  worker protocol.
- Forked child processes with sideband IPC intentionally disabled are
  outside this worker-owned path and may continue to use inherited raw
  output streams.

`output_image`
- `{ "type": "output_image", "image_id": <string>, "mime_type": <string>, "data_b64": <base64>, "update": <bool> }`
- Carries worker-owned image bytes on the ordered sideband stream.
- `image_id` is worker-local source identity for update grouping. The
  server owns MCP response image IDs.

`plot_image`
- `{ "type": "plot_image", "mime_type": <string>, "data": <base64>, "is_update": <bool>, "source": <string|null> }`
- Built-in adapters use this event for plot output.
- There is no plot-image acknowledgement message.
- Workers must not delay stdout/stderr output waiting for sideband responses.

`session_end`
- `{ "type": "session_end", "reason": <string>, "message_b64": <base64, optional>, "input_id": <integer, optional> }`
- Indicates the worker session is terminating.
- `reason` is required for protocol workers. Recognized values are
  `shutdown`, `reset`, `runtime_exit`, `crash`, and `protocol_error`.
- If `input_id` is present and matches the active input, it is terminal
  for that input batch. If it is absent, it is terminal for the whole
  session, including any active input.
- After this event, the worker must not emit more output.

## Interrupt Recovery

Interrupt recovery is worker-owned and fail-closed. The server may send
an interrupt while an input batch is active and may return partial
output if recovery times out. The server must not send new non-empty
input to the same worker until the active input reaches `input_wait`,
reaches `session_end`, or the worker is replaced.

An interrupt for a queued worker works like this:

1. Server sends `{ "type": "interrupt", "input_id": 7 }`.
2. Server may also deliver the platform interrupt to the worker process
   or process group.
3. Worker receives `interrupt(7)`.
4. Worker verifies that `7` is still the active input. If not, the
   worker must not apply the interrupt to a newer input batch.
5. Worker stops writing any not-yet-written queued input for input batch
   `7`.
6. Worker drops any queued line items for input batch `7` that have not
   reached the runtime transport.
7. Worker attempts runtime-specific cleanup for input that may already
   be in the transport, such as draining readable PTY input, flushing
   terminal input, or asking the runtime to unwind.
8. Runtime either reaches a worker-observed input wait, exits, or
   remains busy.
9. If the runtime reaches input wait and the worker can prove no input
   from input batch `7` remains queued, in flight, buffered, or
   cleanup-uncertain, worker sends
   `{ "type": "input_wait", "input_id": 7, "prompt": <string> }`.
10. If the runtime exits, worker sends `session_end`.
11. If cleanup is uncertain, worker sends no `input_wait`. The active
    input remains active until timeout, later recovery, `session_end`,
    or server replacement.

The worker may use runtime-specific cleanup mechanisms internally. Those
details are not protocol facts. If cleanup is only best-effort and old
input may still be buffered in a PTY, libc, readline, or interpreter
state, the worker must not emit `input_wait` for the interrupted input
batch.

## Notes

- Raw stdout/stderr capture remains active for unowned output, such as
  child processes or direct file-descriptor writes. Raw capture must not
  drive completion, prompt detection, echo suppression, or interrupt
  routing.
- For PTY-backed workers, raw visible output may arrive from one
  terminal stream with terminal behavior such as CRLF translation, echo,
  terminal-width effects, and merged stdout/stderr identity.
  Worker-owned `output_text` frames preserve their declared stream; raw
  PTY output does not promise pipe-style stream fidelity.
- Control-only interrupts are server-owned routing decisions: if a
  worker process already exists, the server forwards the interrupt to
  it; if no worker exists, the server must not spawn one only to
  interrupt nothing.
