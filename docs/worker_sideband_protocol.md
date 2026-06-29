# Worker Sideband Protocol

This document defines worker protocol version 8. The server rejects unsupported
protocol versions before sending user input.

The sideband is a UTF-8 JSON-lines IPC stream between the server and one worker
process. Built-in R, built-in Python, and custom workers use the same transport
contract: the server treats workers as opaque runtimes, sends accepted input with
`input_batch`, and waits for the worker to report when it can accept more input.

## Core Contract

The server may send non-empty input only after the worker emits `input_wait` or
`ready`. Those events mean the runtime is waiting at a managed input boundary or
has reached prompt-free readiness, and the worker is ready for the next
`input_batch`.

An input batch is one accepted non-empty MCP `repl()` payload. Empty polls drain
output or report current state; they do not create input batches.

The server owns request lifecycle, timeout handling, raw output capture, MCP
reply construction, output bundles, and server-assigned image IDs. The worker
owns the runtime input queue and decides how input reaches R, Python, or a
custom runtime.

Prompt strings are display/cache data. The server does not infer readiness,
input routing, completion, or interrupt behavior from prompt text, raw stdout,
raw stderr, PTY state, stdin writes, or timing.

## Transport

- Logical sideband endpoints are named by direction: server-to-worker and
  worker-to-server. The bootstrap environment variable names intentionally stay
  transport-specific because Unix carries inherited file-descriptor numbers and
  Windows carries named-pipe paths. This keeps source searches and diagnostics
  tied to the platform primitive actually being bootstrapped.
- Unix endpoints:
  - `MCP_REPL_IPC_READ_FD`: worker reads server-to-worker messages
  - `MCP_REPL_IPC_WRITE_FD`: worker writes worker-to-server messages
- Windows endpoints:
  - `MCP_REPL_IPC_PIPE_TO_WORKER`: worker reads server-to-worker messages
  - `MCP_REPL_IPC_PIPE_FROM_WORKER`: worker writes worker-to-server messages
- Messages are serialized as one JSON object per line.
- Worker-owned text, images, input facts, and lifecycle facts are ordered on the
  sideband stream.
- Raw stdout/stderr or PTY capture remains active for output that bypasses the
  worker-owned sideband path.
- Sideband IPC is implemented only for Unix-family systems and Windows. There
  is no third transport family today.

For built-in workers on Unix and Windows, the launch environment variables are
bootstrap-only. The worker consumes and removes them while connecting to IPC,
before runtime user code runs.

On Unix, built-in workers register an at-fork handler that disables sideband IPC
in forked children and closes inherited sideband file descriptors. Those
children may still write inherited raw stdout/stderr, but they must not emit
sideband messages or consume managed queued input. Python's managed stdin
surfaces return EOF in sideband-disabled fork children. R's `ReadConsole`
callback does not currently implement that EOF policy in fork children; aligning
it is pending implementation work. Raw inherited fd0 that bypasses managed stdin
is unsupported by the worker protocol.

PTY-backed Unix workers expose one raw terminal stream to the server, so raw PTY
capture does not preserve separate stdout/stderr identity. Sideband
`output_text` still preserves its declared stream.

## Server To Worker

`input_batch`
- `{ "type": "input_batch", "input": <string> }`
- Sends one accepted non-empty client payload.
- The server sends this only after `input_wait` or `ready` has made the worker
  ready for input. Sending it marks the worker not ready for input.
- The worker owns input normalization, such as appending a final newline for
  line-oriented runtimes.

`interrupt`
- `{ "type": "interrupt", "interrupt_id": <integer> }`
- Sent when the client requests interrupt and a worker IPC endpoint exists.
- The server sends this cleanup message first, waits briefly for
  `interrupt_ack` carrying the same `interrupt_id`, and then delivers a
  platform interrupt to the worker process or process group when a worker
  process exists. The platform interrupt is sent whether the ack arrives or
  times out.
- The IPC message carries no input and does not complete a batch. It tells the
  worker to discard pending managed input that has not yet been consumed by the
  runtime.
- `interrupt_id` is assigned by the server and is scoped to the worker
  connection. Workers must echo it in the matching `interrupt_ack`.
- Sending `interrupt` does not change server-side readiness. Readiness changes
  only when the server sends `input_batch` or receives `input_wait` or `ready`.
- While servicing an interrupt request, the server waits for `input_wait`,
  `session_end`, process exit, or timeout.

`shutdown`
- `{ "type": "shutdown" }`
- Requests worker shutdown during server teardown and replacement paths that
  need worker-side lifecycle control. The worker should stop accepting new
  input, preserve already accepted input until the runtime consumes it, wake
  managed stdin readers with EOF once that accepted input is drained, let the
  active runtime request reach a safe boundary, and exit. The server waits
  through a bounded graceful shutdown window, then escalates if needed. The
  server may also close process stdin during that window for worker types that
  rely on stdin EOF to unblock direct stdin consumers; built-in Python keeps
  process stdin open during the graceful window because sideband owns accepted
  input. Restart replies include output captured through that window, followed
  by fresh-session tail output when the same input contains text after
  `Ctrl-D`.
- Built-in workers request runtime shutdown after receiving it. They may emit
  output from the active request before `session_end` and process exit.

The server emits no other server-to-worker protocol messages in v7.
Built-in worker readers treat malformed or unknown server-to-worker messages as
IPC loss and exit so the server can replace the worker.

## Worker To Server

Worker-to-server messages are strict. Unknown fields, unknown message types,
invalid enum values, and invalid base64 payloads in base64 fields are protocol
errors.

`worker_ready`
- `{ "type": "worker_ready", "protocol": { "name": "mcp-repl-worker", "version": 8 }, "worker": { "name": <string>, "version": <string> }, "capabilities": { "images": <bool> } }`
- Normal first worker message.
- `protocol.name` must be `mcp-repl-worker`, and `protocol.version` must be `8`.
- `worker.name` and `worker.version` are diagnostic metadata.
- `capabilities.images` is advertised by workers that may emit image output; if
  the field is omitted inside `capabilities`, the server treats it as `false`.
- A worker may instead end before readiness; the server treats early
  `session_end`, IPC EOF, or process exit as startup failure.
- Any other first worker-to-server message is a protocol error.

`input_wait`
- `{ "type": "input_wait", "prompt": <string> }`
- Marks the worker ready for the next `input_batch`.
- If an input batch is active, this is the successful same-worker completion
  signal for that batch.
- If no input batch is active, this only refreshes readiness and the prompt
  cache. Built-in workers use this for the initial ready prompt.

`ready`
- `{ "type": "ready" }`
- Marks the worker ready for the next `input_batch` without a visible prompt.
- If an input batch is active, this is the successful same-worker completion
  signal for that batch.
- If no input batch is active, this permits prompt-free startup readiness.

`interrupt_ack`
- `{ "type": "interrupt_ack", "interrupt_id": <integer>, "discarded_input": <bool> }`
- Confirms that the worker processed the ordered sideband `interrupt` cleanup
  message with the same `interrupt_id`.
- `discarded_input` is `true` when pending managed input was discarded before
  the runtime consumed it. It is `false` when no pending managed input existed.
- This message does not mark the worker ready, complete active input, interrupt
  runtime code, or change server-side readiness.

`input_line`
- `{ "type": "input_line", "prompt": <string>, "text": <string> }`
- Records that the worker delivered accounted input to the runtime at this
  point in the ordered sideband stream.
- Valid only while an input batch is active and before that batch's
  `input_wait`.
- `prompt` and `text` are structural input facts, not runtime output.
- Submitted input must not be emitted as `output_text`.
- The server may reconstruct `prompt + text` from ordered `input_line` events
  for transcript finalization, debugging views, or other surfaces that need an
  interactive transcript.

`output_text`
- `{ "type": "output_text", "stream": <"stdout"|"stderr">, "data_b64": <base64>, "is_continuation": <bool, optional> }`
- Carries worker-owned output bytes on the ordered sideband stream.
- Worker-owned runtime output must be emitted only on sideband IPC, not also to
  raw stdout/stderr. Raw capture is only for output the worker cannot account
  for, such as forked children, subprocesses, or direct file-descriptor writes
  that bypass the worker-owned output path.
- `is_continuation` defaults to `false`. It exists because one worker-owned
  write may be split into bounded IPC frames; `true` marks frames that continue
  the same logical write.
- Prompt-looking bytes are ordinary output unless the worker reports them in
  `input_wait.prompt` or `input_line.prompt`.
- Workers must write, newline-terminate, and flush sideband output frames before
  returning from the emitting call. Workers must not delay unowned raw
  stdout/stderr output waiting for sideband responses.

`output_image`
- `{ "type": "output_image", "mime_type": <string>, "data_b64": <base64>, "is_update": <bool>, "source": <string|null> }`
- Carries worker-owned image bytes on the ordered sideband stream.
- `source` is optional worker-local identity for update grouping. It is not a
  server-visible image ID.
- The server validates `data_b64`, assigns response image IDs, and uses
  `source` plus `is_update` to group image updates.
- Built-in R and Python plot adapters emit plot results with this generic image
  event. Plotting is runtime behavior, not a separate protocol category.
- There is no image acknowledgement message.

`session_end`
- `{ "type": "session_end", "reason": <string>, "message": <string, optional> }`
- Indicates the worker session is terminating.
- `reason` is optional in the current implementation. If present, it must be one
  of `shutdown`, `reset`, `runtime_exit`, `crash`, or `protocol_error`.
- `message`, if present, is UTF-8 JSON string text for diagnostics.
- This is terminal for the whole worker session, including any active input.
  After `session_end`, any later worker-to-server message is a protocol error.

The worker emits no other worker-to-server protocol messages in v7.

## Readiness And Input

The worker starts not ready for input. The server waits for `worker_ready`, then
waits for the first `input_wait` or `ready`. Only then may it send an
`input_batch`.

After the server sends `input_batch`, it considers the worker not ready. The
worker queues the batch and wakes its managed runtime input path. When the
runtime consumes all queued input and asks the managed input boundary for more,
the worker emits `input_wait`; a prompt-free top-level loop may instead emit
`ready`.

Built-in R and Python use the same ownership model: a worker-owned managed input
queue feeds runtime input callbacks and managed stdin surfaces. The sideband IPC
reader runs independently from the runtime thread. It receives `input_batch`,
`interrupt`, and `shutdown`, mutates worker-owned queue/session state, emits
`interrupt_ack` after cleanup, and wakes the runtime when needed. Runtime
interruption comes from the server's platform interrupt delivery, not from the
sideband `interrupt` message. The runtime thread consumes queued input only when
the runtime calls its managed input boundary. A `shutdown` message asks built-in
workers to stop accepting new input, preserve already accepted input until it
is consumed, wake managed input readers with EOF after that queue drains, and
exit after the active runtime request reaches a safe boundary or the server's
bounded shutdown window expires. The server still applies the worker's process
stdin shutdown policy so stdin-backed readers can be unblocked without making
stdin EOF the only shutdown signal.

For R, the managed input boundary is R's embedded `ReadConsole` callback,
installed through `Rstart.ReadConsole` on Windows and `ptr_R_ReadConsole` on
Unix. `ReadConsole` runs on the R runtime thread. It removes queued lines or
buffer-sized fragments, emits `input_line` for delivered text, and emits
`input_wait` when the active batch has drained and R asks for more input.

For Python, the primary REPL input boundary is CPython's
`PyOS_ReadlineFunctionPointer`. It calls the worker's readline callback on the
Python runtime thread. The Python bootstrap also installs managed `sys.stdin`
wrappers and raw-stdin shims for code paths that read from `sys.stdin`, file
objects, `os.read(0, ...)`, or equivalent fd-0 aliases. Those surfaces all draw
from the same worker-owned queue and share the same accounting; they are bridges
to the managed queue, not independent input sources. Each delivered line or
byte-oriented managed read emits `input_line`; when the queue is empty after an
active batch, the worker emits `input_wait` or `ready`.

Built-in Python uses prompt-free cell execution at top level. At the start of a
non-empty tool call, an existing `input_wait` means the payload is stdin for the
waiting Python reader; otherwise the payload is one complete Python cell.
`ready` with no prompt is normal cell readiness, not a missing prompt.
`input_wait` is the only public signal that the next non-empty payload is
stdin. After interrupt, the server must wait for fresh `ready`, but an already
pending `input_wait` remains actionable because it still denotes a waiting
stdin reader.

Python may still use PTY or ConPTY process stdio for terminal behavior, but
accepted request input is sent over sideband IPC, not by server writes to
runtime stdin.

Custom workers must implement the same readiness contract themselves. The test
Zod worker exercises the custom-worker input/readiness path: it sends
`worker_ready`, sends an initial `input_wait` or `ready`, accepts `input_batch`,
emits `input_line` for consumed lines, and emits a later `input_wait` or `ready`
when it is ready again. It is not a full implementation of every optional
protocol surface.

## Interrupts

Interrupt must not make a worker look ready by assumption. The server sends
`interrupt` whenever the client asks for interrupt and a worker endpoint exists,
and it must also send the platform interrupt when a worker process exists. The
sideband `interrupt` message is the worker-owned cleanup signal; the platform
interrupt is delivered to the runtime or process group, and the runtime handles
it according to its own rules.

When the worker receives sideband `interrupt`, it must discard managed input
that is still queued and has not yet been consumed by the runtime. The discard
is triggered by the IPC message, not by the OS interrupt. The worker must not
emit `input_wait` again until it is actually ready for input.

Current built-in behavior:

- R and Python both discard queued managed input that has not yet reached their
  managed input boundary.
- For R, that boundary is `ReadConsole`, so already-returned text is owned by R.
- For Python, those boundaries are `PyOS_ReadlineFunctionPointer`, managed
  `sys.stdin`, and raw-stdin shims, so already-returned bytes are owned by
  CPython or the Python code that read them.
- The server sends `SIGINT` on Unix and `CTRL_BREAK_EVENT` on Windows to the
  worker process group.

If cleanup is uncertain because old input may still be buffered in PTY, libc,
readline, or interpreter state, the worker must not emit `input_wait`. It should
remain not ready until later recovery, `session_end`, timeout, or server
replacement.

## Current Limitations And Historical Messages

Unsupported direct fd0 readers remain unsupported unless the worker routes them
through the managed input path. Raw stdout/stderr remains authoritative for
output that did not arrive through `output_text`; raw capture does not drive
completion or readiness.

The following older protocol concepts are not part of v7:

- input IDs on `input_batch`, `input_line`, `input_wait`, `interrupt`, or
  `session_end`
- `turn_start` and `turn_input`
- `readline_start`
- `idle` and `stdin_wait`
- `plot_image`
- old image fields `image_id` and `update`
- old `session_end.message_b64`
- image sequence or acknowledgement handshakes

There is no compatibility path for older protocol versions.

Pending or aspirational design note: richer non-plot display objects could use
the existing `output_image` event, but the built-in R and Python workers
currently emit images only through their plot adapters.

Platform note: Windows Python support depends on a loadable CPython runtime and
uses ConPTY for terminal behavior. Sideband named pipes remain separate from
ConPTY traffic.
