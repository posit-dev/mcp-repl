# Worker Sideband Protocol

This document defines worker protocol version 6. The server rejects unsupported
protocol versions before sending user input.

The sideband is a UTF-8 JSON-lines IPC stream between the server and one worker
process. Built-in R, built-in Python, and custom workers use the same transport
contract: the server treats workers as opaque runtimes, sends accepted input with
`input_batch`, and waits for the worker to report when it can accept more input.

## Core Contract

The server may send non-empty input only after the worker emits `input_wait`.
That event means the runtime is waiting at a managed input boundary and the
worker is ready for the next `input_batch`.

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
  worker-to-server. The bootstrap environment variable names stay
  transport-specific because Unix carries inherited file-descriptor numbers and
  Windows carries named-pipe paths.
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
sideband messages or consume managed queued input. Managed stdin for
sideband-disabled fork children should return EOF where the worker controls that
stdin surface; Python fork children currently do this. Raw inherited fd0 that
bypasses managed stdin remains outside the sideband contract.

PTY-backed Unix workers expose one raw terminal stream to the server, so raw PTY
capture does not preserve separate stdout/stderr identity. Sideband
`output_text` still preserves its declared stream.

## Server To Worker

`input_batch`
- `{ "type": "input_batch", "input": <string> }`
- Sends one accepted non-empty client payload.
- The server sends this only after `input_wait` has made the worker ready for
  input. Sending it marks the worker not ready for input.
- The worker owns input normalization, such as appending a final newline for
  line-oriented runtimes.

`interrupt`
- `{ "type": "interrupt" }`
- Sent when the client requests interrupt and a worker IPC endpoint exists.
- The server must also deliver a platform interrupt to the worker process or
  process group when a worker process exists.
- The IPC message carries no input and does not complete a batch. It tells the
  worker to discard pending managed input that has not yet been consumed by the
  runtime.
- Sending `interrupt` does not change server-side readiness. Readiness changes
  only when the server sends `input_batch` or receives `input_wait`.
- While servicing an interrupt request, the server waits for `input_wait`,
  `session_end`, process exit, or timeout.

`shutdown`
- `{ "type": "shutdown" }`
- Requests worker shutdown during reset, replacement, or server teardown.
- Built-in workers currently exit the process after receiving it.

The server emits no other server-to-worker protocol messages in v6.
Built-in worker readers treat malformed or unknown server-to-worker messages as
IPC loss and exit so the server can replace the worker.

## Worker To Server

Worker-to-server messages are strict. Unknown fields, unknown message types,
invalid enum values, and invalid base64 payloads in base64 fields are protocol
errors.

`worker_ready`
- `{ "type": "worker_ready", "protocol": { "name": "mcp-repl-worker", "version": 6 }, "worker": { "name": <string>, "version": <string> }, "capabilities": { "images": <bool> } }`
- Normal first worker message.
- `protocol.name` must be `mcp-repl-worker`, and `protocol.version` must be `6`.
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

The worker emits no other worker-to-server protocol messages in v6.

## Readiness And Input

The worker starts not ready for input. The server waits for `worker_ready`, then
waits for the first `input_wait`. Only then may it send an `input_batch`.

After the server sends `input_batch`, it considers the worker not ready. The
worker queues the batch and wakes its managed runtime input path. When the
runtime consumes all queued input and asks the managed input boundary for more,
the worker emits `input_wait`.

Built-in R and Python use the same model: a worker-owned managed input queue
feeds runtime input callbacks and managed stdin surfaces. In R this is the
embedded `ReadConsole` callback. In Python this includes CPython readline plus
managed `sys.stdin` and raw-stdin shims. Each delivered line or byte-oriented
managed read emits `input_line`; when the queue is empty after an active batch,
the worker emits `input_wait`. Python may still use PTY or ConPTY process stdio
for terminal behavior, but accepted request input is sent over sideband IPC, not
by server writes to runtime stdin.

Custom workers must implement the same readiness contract themselves. The test
Zod worker exercises the custom-worker input/readiness path: it sends
`worker_ready`, sends an initial `input_wait`, accepts `input_batch`, emits
`input_line` for consumed lines, and emits a later `input_wait` when it is ready
again. It is not a full implementation of every optional protocol surface.

## Interrupts

Interrupt is fail-closed. The server sends `interrupt` whenever the client asks
for interrupt and a worker endpoint exists, and it must also send the platform
interrupt when a worker process exists. The sideband `interrupt` message is the
worker-owned cleanup signal; the platform interrupt is delivered to the runtime
or process group, and the runtime handles it according to its own rules.

When the worker receives sideband `interrupt`, it must discard managed input
that is still queued and has not yet been consumed by the runtime. The discard
is triggered by the IPC message, not by the OS interrupt. The worker must not
emit `input_wait` again until it is actually ready for input.

Current built-in behavior:

- R drops queued lines that have not yet reached `ReadConsole`.
- Python clears or drains its managed input queue where that queue has not yet
  reached the runtime.
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

The following older protocol concepts are not part of v6:

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
