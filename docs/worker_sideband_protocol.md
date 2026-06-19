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

- Unix: the worker inherits two file descriptors:
  - `MCP_REPL_IPC_READ_FD`
  - `MCP_REPL_IPC_WRITE_FD`
- Windows: the worker connects to two server-created byte-mode named pipes:
  - `MCP_REPL_IPC_PIPE_TO_WORKER`
  - `MCP_REPL_IPC_PIPE_FROM_WORKER`
- Messages are serialized as one JSON object per line.
- Worker-owned text, images, input facts, and lifecycle facts are ordered on the
  sideband stream.
- Raw stdout/stderr or PTY capture remains active for output that bypasses the
  worker-owned sideband path.
- Sideband IPC is implemented only for Unix-family systems and Windows.

On Unix, the launch environment variables are bootstrap-only. The worker
consumes them when connecting to IPC, and server launch code removes them from
the user-code environment before runtime code runs. On Windows, the pipe-name
environment variables are read during connection but are not currently removed
afterward.

On Unix, built-in workers register an at-fork handler that disables sideband IPC
in forked children and closes inherited sideband file descriptors. Those
children may still write inherited raw stdout/stderr, but they must not emit
sideband messages or consume managed queued input. Python fork children
currently see EOF for managed stdin.

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
- The server may also deliver a platform interrupt to the worker process or
  process group.
- The message carries no input and does not complete a batch. After sending it,
  the server marks the worker not ready for input and waits for `input_wait`,
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
invalid enum values, and invalid base64 payloads are protocol errors.

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
- Built-in R also mirrors console echo as `output_text` after `input_line`, and
  the server trims or reconstructs that echo for final presentation.
- The server may reconstruct `prompt + text` from ordered `input_line` events
  for transcript finalization, echo trimming, debugging views, or other
  surfaces that need an interactive transcript.

`output_text`
- `{ "type": "output_text", "stream": <"stdout"|"stderr">, "data_b64": <base64>, "is_continuation": <bool, optional> }`
- Carries worker-owned output bytes on the ordered sideband stream.
- `is_continuation` defaults to `false` and marks transport chunks that continue
  the same worker-owned write.
- Prompt-looking bytes are ordinary output unless the worker reports them in
  `input_wait.prompt` or `input_line.prompt`.
- Workers must write, newline-terminate, and flush sideband output frames before
  returning from the emitting call. Workers must not delay stdout/stderr output waiting for sideband responses.

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
- `{ "type": "session_end", "reason": <string>, "message_b64": <base64, optional> }`
- Indicates the worker session is terminating.
- `reason` is optional in the current implementation. If present, it must be one
  of `shutdown`, `reset`, `runtime_exit`, `crash`, or `protocol_error`.
- `message_b64`, if present, must be valid base64.
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

R currently implements this with the embedded `ReadConsole` callback and a
worker-owned queue of input lines. Each delivered line or buffer-sized fragment
emits `input_line`; when the queue is empty after an active batch,
`ReadConsole` emits `input_wait`.

Python currently uses managed input queues as well. On Unix, the queue feeds
CPython readline, managed `sys.stdin`, and raw-stdin shims; on Windows, the
queue feeds tracked readline/stdin paths and checks pending bytes on the
runtime stdin pipe. Python `input_line` can represent a line or a byte-oriented
managed read. Python may still use PTY or ConPTY process stdio for terminal
behavior, but accepted request input is sent over sideband IPC, not by server
writes to runtime stdin.

Custom workers must implement the same readiness contract themselves. The test
Zod worker exercises the custom-worker input/readiness path: it sends
`worker_ready`, sends an initial `input_wait`, accepts `input_batch`, emits
`input_line` for consumed lines, and emits a later `input_wait` when it is ready
again. It is not a full implementation of every optional protocol surface.

## Interrupts

Interrupt is fail-closed. The server sends `interrupt` whenever the client asks
for interrupt and a worker endpoint exists, and it may also send the platform
interrupt. The worker must not emit `input_wait` again until it is actually
ready for input.

Current built-in behavior:

- R drops queued lines that have not yet reached `ReadConsole`; the server also
  sends the R-specific process interrupt, such as `SIGINT` on Unix or
  `CTRL_BREAK_EVENT` on Windows.
- Python sets worker-side interrupt state and uses runtime/platform interrupt
  paths. Unix Python also clears its managed input queue after interrupt; Windows
  Python drains pending stdin pipe bytes when it can. On Windows, Python and
  custom protocol workers rely on the sideband interrupt rather than an
  additional server-delivered console control event.

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
- image sequence or acknowledgement handshakes

There is no compatibility path for older protocol versions.

Pending or aspirational design note: richer non-plot display objects could use
the existing `output_image` event, but the built-in R and Python workers
currently emit images only through their plot adapters.

Platform note: Windows Python support depends on a loadable CPython runtime and
uses ConPTY for terminal behavior. Sideband named pipes remain separate from
ConPTY traffic.
