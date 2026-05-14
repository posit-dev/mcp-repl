# Worker Sideband Protocol (JSON Lines)

This document describes the sideband protocol between the server and a worker
process. The channel is a UTF-8 JSON-lines stream, one JSON object per line,
carried over an IPC pipe.

User input is not carried on sideband. The server writes decoded MCP `repl()`
text to worker stdin, appending exactly one trailing `\n` to non-empty input
that does not already end in `\n`. The worker owns runtime stdin placement and
reports stdin accounting facts back on sideband.

## Transport

- Unix: worker inherits two file descriptors through environment variables:
  - `MCP_REPL_IPC_READ_FD`
  - `MCP_REPL_IPC_WRITE_FD`
- Windows: worker connects to two server-created named pipes through
  environment variables:
  - `MCP_REPL_IPC_PIPE_TO_WORKER`
  - `MCP_REPL_IPC_PIPE_FROM_WORKER`
- Messages are serialized as UTF-8 JSON, one message per line.

## Direction: server -> worker

`interrupt`
- `{ "type": "interrupt" }`
- Sent when the server is about to issue an OS interrupt to an existing worker
  process or process group.
- This is for worker-owned bookkeeping only. It does not carry user input and
  does not replace the OS interrupt.
- The worker may emit `readline_discard` for exact active-turn stdin bytes it
  discarded before delivering them to the runtime.

`session_end`
- `{ "type": "session_end" }`
- Sent when the server is ending the current session, for example during reset
  or shutdown.
- Worker treats this as shutdown intent and stops consuming further stdin
  payloads.

## Direction: worker -> server

Worker-to-server messages are strict: unknown fields, invalid enum values,
invalid base64, and unknown message types are protocol errors.

`worker_ready`
- `{ "type": "worker_ready", "protocol": { "name": "mcp-repl-worker", "version": 1 }, "worker": { "name": <string>, "version": <string> }, "capabilities": { "images": <bool> }, "graceful_shutdown": { "stdin": <string> } }`
- Must be the first worker-to-server message for protocol workers.
- The server rejects unsupported protocol names or versions before sending user
  input.
- `worker.name` is diagnostic metadata. Server request handling must not branch
  on it.
- `graceful_shutdown` is optional. If present, the server may write that exact
  stdin text only when server-owned shutdown/reset rules decide a graceful
  stdin request is appropriate.

`readline_start`
- `{ "type": "readline_start", "prompt": <string> }`
- Emitted when the runtime enters a line-read operation, before reading bytes
  for that operation.
- The prompt string is required; use an empty string if the runtime supplied no
  prompt.
- If active-turn stdin bytes remain unaccounted, the prompt is satisfied by
  already-written stdin and does not complete the request. If no active-turn
  stdin bytes remain, the prompt is unsatisfied and may complete the request.
- Prompt rendering is derived from this structured event, not from raw
  stdout/stderr parsing.

`readline_input`
- `{ "type": "readline_input", "text": <string> }`
- Emitted after the worker delivers active-turn stdin text to the
  runtime-facing input layer.
- The server encodes `text` as UTF-8 and removes those bytes from the active
  stdin queue. A mismatch is a protocol error.

`readline_discard`
- `{ "type": "readline_discard", "text": <string> }`
- Emitted after the worker discards active-turn stdin text during
  interrupt/reset cleanup without delivering it to the runtime.
- The server encodes `text` as UTF-8 and removes those bytes from the active
  stdin queue. A mismatch is a protocol error.

`output_text`
- `{ "type": "output_text", "stream": <"stdout"|"stderr">, "data_b64": <base64>, "is_continuation": <bool, optional> }`
- Carries worker-owned output bytes on the ordered sideband stream. The payload
  is base64 so workers can preserve bytes without depending on JSON string
  encoding.
- Prompt-looking bytes are ordinary output unless the worker reports them in
  `readline_start.prompt`.
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

`session_end`
- `{ "type": "session_end", "reason": <string>, "message_b64": <base64, optional> }`
- Indicates the worker session is terminating.
- `reason` is required for protocol workers. Recognized values are `shutdown`,
  `reset`, `runtime_exit`, `crash`, and `protocol_error`.
- After this event, the worker must not emit more output.

## Transitional Compatibility Frames

These frames remain for built-in workers that have not fully migrated on every
platform. New protocol workers, Zod, and Unix Python should not use them for
steady-state request handling.

`stdin_write`
- `{ "type": "stdin_write", "byte_len": <usize>, "line_count": <usize>, "final_prompt": <string, optional> }`
- Legacy server-to-worker request metadata emitted before the server writes raw
  input payload bytes to stdin.
- Built-in R still uses `byte_len` to make its worker-owned stdin reader consume
  exactly one raw payload before handing it to embedded R.
- Non-Unix Python may still use `line_count` and `final_prompt` until it is
  migrated to the protocol-worker stdin accounting model.

`stdin_write_complete`
- `{ "type": "stdin_write_complete" }`
- Legacy server-to-worker marker emitted after the server has written the raw
  input payload bytes to stdin.

`backend_info`
- `{ "type": "backend_info", "supports_images": <bool> }`
- Legacy startup metadata accepted from older built-in workers.
- It may describe narrow worker capabilities, but it must not turn steady-state
  server request handling into language-specific policy.

`stdin_write_ack`
- `{ "type": "stdin_write_ack" }`
- Legacy worker-to-server request-boundary acknowledgement.
- This only acknowledges request-boundary state. It is not an acknowledgement
  for stdout/stderr, plot images, or request completion.

`readline_result`
- `{ "type": "readline_result", "prompt": <string>, "line": <string> }`
- Legacy echo metadata emitted after a line is read.
- The server may use it for conservative echo suppression of raw pipe output,
  but completion is driven by `readline_start`, `readline_input`,
  `readline_discard`, and `session_end`.

`plot_image`
- `{ "type": "plot_image", "mime_type": <string>, "data": <base64>, "is_update": <bool>, "source": <string|null> }`
- Legacy image payload used by built-in plot emitters.
- `source` is optional worker-local plot source identity, such as a graphics
  device or figure slot. It is not a response image ID; the server owns response
  image IDs and uses `source` only to keep distinct plot sources from
  collapsing into one response image.
- There is no plot-image acknowledgement message.
- Workers must not delay stdout/stderr output waiting for sideband responses.
- If an update is the first image event for a new server request, the server
  treats it as a new response image and includes a server notice that it updates
  the previously sent image.

## Notes

- Raw stdout/stderr capture remains active for unowned output, such as child
  processes or direct file-descriptor writes. Raw capture must not drive
  completion, prompt detection, echo suppression, or interrupt routing.
- The server infers request completion when explicit worker sideband facts show
  that the worker is waiting for the next input or that the session ended.
- On timeout, a request may remain pending; later empty polls can observe worker
  events and finish the request.
- Control-only interrupts are server-owned routing decisions: if a worker
  process already exists, the server forwards the interrupt to it; if no worker
  exists, the server must not spawn one only to interrupt nothing.
