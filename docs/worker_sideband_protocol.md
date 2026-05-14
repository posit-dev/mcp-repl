# Worker Sideband Protocol (JSON Lines)

This document describes the minimal sideband protocol between the server and a
worker process. The channel is a JSON-lines stream (one JSON object per line)
carried over an IPC pipe.

## Transport

- Availability:
  - Unix: worker inherits two file descriptors via environment variables:
    - `MCP_REPL_IPC_READ_FD`
    - `MCP_REPL_IPC_WRITE_FD`
  - Windows: worker connects to two server-created named pipes via environment
    variables:
    - `MCP_REPL_IPC_PIPE_TO_WORKER`
    - `MCP_REPL_IPC_PIPE_FROM_WORKER`
- Messages are serialized as UTF-8 JSON, one message per line.

## Direction: server -> worker

`stdin_write`
- `{ "type": "stdin_write", "byte_len": <usize>, "line_count": <usize>, "final_prompt": <string, optional> }`
- Emitted before the server writes the raw input payload bytes to stdin.
- The payload itself is not carried on IPC and stdin contains no protocol
  header.
- R worker mode uses `byte_len` to make its worker-owned stdin reader consume
  exactly one raw payload before handing it to embedded R.
- Python worker mode lets CPython own stdin. It ignores `byte_len` after request
  acceptance and uses `line_count` to know how many CPython readline calls
  belong to the active request.
- `line_count` is optional for older senders and defaults to `0`.
- `final_prompt` is optional and is currently sent by the Python backend driver.
  It is a best-effort fallback prompt hint for the request payload, used only
  while the worker is still waiting for the current request to finish.

`stdin_write_complete`
- `{ "type": "stdin_write_complete" }`
- Emitted after the server has written the raw input payload bytes to stdin.
- Python worker mode uses this frame to distinguish "no more bytes will arrive
  for this request" from "stdin may still be draining", so it can finish a
  request only after both CPython readline progress and payload write completion
  agree.
- R worker mode currently ignores this frame because the R stdin reader owns
  exact payload consumption through `byte_len`.

`interrupt`
- `{ "type": "interrupt" }`
- Sent when the server issues an interrupt to an existing worker process.
- Interrupt routing must not depend on prompt text. Prompt strings are visible
  output data, not control metadata; for example, a user-code prompt may look
  like a primary REPL prompt or may be empty.
- For R, worker-side handlers clear any pending queued input.
- For Python, the worker invokes CPython's interrupt API and discards pending
  stdin bytes from the current request before normal completion signaling
  resumes.

`session_end`
- `{ "type": "session_end" }`
- Sent when the server is ending the current session (for example
  restart/shutdown).
- Worker treats this as shutdown intent and stops consuming further stdin
  payloads.

## Direction: worker -> server

Worker-to-server messages are strict: unknown fields are protocol errors.

`backend_info`
- `{ "type": "backend_info", "supports_images": <bool> }`
- Sent once on startup after the sideband connection is established.
- This is startup metadata. It may describe narrow worker capabilities, but it
  must not turn steady-state server request handling into language-specific
  policy.

`stdin_write_ack`
- `{ "type": "stdin_write_ack" }`
- Sent after a worker has processed `stdin_write` and installed request metadata
  for the upcoming raw stdin bytes.
- Currently emitted by Python worker mode. R worker mode does not emit it
  because the server does not need an acceptance barrier before writing the raw
  payload to R's worker-owned stdin reader.
- This only acknowledges request-boundary state. It is not an acknowledgement
  for stdout/stderr, plot images, or request completion.
- `stdin_write_ack` is not an output-drain barrier. A future output-drain gate
  should use a separate protocol step.

`readline_start`
- `{ "type": "readline_start", "prompt": <string> }`
- Emitted for readline prompts. The prompt string is required; use an empty
  string if the backend did not supply one.
- If active-turn stdin bytes remain unaccounted, the prompt is treated as
  satisfied by already-written stdin and does not complete the request. If no
  active-turn stdin bytes remain, the prompt is unsatisfied and may complete
  the request.

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

`readline_result`
- `{ "type": "readline_result", "prompt": <string>, "line": <string> }`
- Emitted after a line is read. Includes the prompt and the line that was
  consumed.
- The server can reconstruct echoed readline bytes as `prompt + line` for
  conservative echo suppression of raw pipe output.

`output_text`
- `{ "type": "output_text", "stream": <"stdout"|"stderr">, "data_b64": <base64>, "is_continuation": <bool, optional> }`
- Carries worker-owned text bytes on the ordered IPC stream. The payload is
  base64 so workers can preserve bytes without depending on JSON string
  encoding.
- R uses this for R-owned console output and prompts. Python uses this for
  Python-level `sys.stdout` and `sys.stderr` after installing embedded stream
  objects.
- `is_continuation` marks bounded transport chunks that continue the same
  worker-owned output write. It defaults to `false`.
- Workers send output-critical frames synchronously: each JSON line is written,
  newline-terminated, and flushed before the send returns.
- Workers treat synchronous write failure as IPC failure. They must not silently
  fall back to stdout or stderr for output that is owned by the worker protocol.
- Forked child processes with sideband IPC intentionally disabled are outside
  this worker-owned path and may continue to use inherited raw output streams.

`plot_image`
- `{ "type": "plot_image", "mime_type": <string>, "data": <base64>, "is_update": <bool>, "source": <string|null> }`
- Image payload for plot updates.
- `source` is optional worker-local plot source identity, such as a graphics
  device or figure slot. It is not a response image ID; the server owns response
  image IDs and uses `source` only to keep distinct plot sources from collapsing
  into one response image.
- There is no plot-image acknowledgement message.
- Workers must not delay stdout/stderr output waiting for sideband responses.
- If an update is the first image event for a new server request, the server
  treats it as a new response image and includes a server notice that it updates
  the previously sent image.

`session_end`
- `{ "type": "session_end" }`
- Indicates the worker session is terminating.

## Notes

- Raw stdout/stderr capture remains active for unowned output, such as child
  processes or direct file-descriptor writes.
- The server infers request completion when prompt/readline sideband facts show
  that the worker is waiting for the next input.
- To reduce IPC-vs-output capture races, the server applies a short
  post-completion settle window so output reader threads can drain final bytes
  before snapshotting output.
- A future pre-input drain gate may let the server hold back the next stdin
  payload while it drains raw stdout/stderr from the previous boundary for a
  small bounded budget, for example about 200 ms with a hard stop. Child output
  that arrives after that boundary belongs to the next response. This would be a
  request-boundary coordination step, not a per-output acknowledgement.
- On timeout, a request may remain pending; later polls can observe the inferred
  completion state and finish the request.
- Backend-specific execution rules should be implemented by the worker. Server
  branching on backend is reserved for interpreter selection, launch setup, tool
  description selection, or narrow capabilities reported at startup.
- Control-only interrupts are still server-owned routing decisions: if a worker
  process already exists, the server forwards the interrupt to it; if no worker
  exists, the server must not spawn one only to interrupt nothing.
