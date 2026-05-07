# Worker Sideband Protocol (JSON Lines)

This document describes the minimal sideband protocol between the server and a worker process.
The channel is a JSON-lines stream (one JSON object per line) carried over an IPC pipe.

## Transport

- Availability:
  - Unix: worker inherits two file descriptors via environment variables:
    - `MCP_REPL_IPC_READ_FD`
    - `MCP_REPL_IPC_WRITE_FD`
  - Windows: worker connects to two server-created named pipes via
    environment variables:
    - `MCP_REPL_IPC_PIPE_TO_WORKER`
    - `MCP_REPL_IPC_PIPE_FROM_WORKER`
- Messages are serialized as UTF-8 JSON, one message per line.

## Direction: server -> worker

`stdin_write`
- `{ "type": "stdin_write", "text": <string> }`
- Emitted before the server writes the input payload to stdin.

`interrupt`
- `{ "type": "interrupt" }`
- Sent when the server issues an interrupt.
- For R, worker-side handlers clear any pending queued input.
- Indicates that readline/prompt handling should discard any remaining buffered stdin bytes
  from the pending request before normal completion signaling resumes.

`session_end`
- `{ "type": "session_end" }`
- Sent when the server is ending the current session (for example restart/shutdown).
- Worker treats this as shutdown intent and stops consuming further stdin payloads.

## Direction: worker -> server

Worker-to-server messages are strict: unknown fields are protocol errors.

`backend_info`
- `{ "type": "backend_info", "supports_images": <bool> }`
- Sent once on startup after the sideband connection is established.
- This is startup metadata. It may describe narrow worker capabilities, but it
  must not turn steady-state server request handling into language-specific
  policy.

`readline_start`
- `{ "type": "readline_start", "prompt": <string> }`
- Emitted before each readline call to indicate the prompt that will be shown.
  The prompt string is required; use an empty string if the backend did not supply one.

`readline_result`
- `{ "type": "readline_result", "prompt": <string>, "line": <string> }`
- Emitted after a line is read. Includes the prompt and the line that was consumed.
- The server can reconstruct echoed readline bytes as `prompt + line` for conservative
  echo suppression. Output streams remain unframed.

`plot_image`
- `{ "type": "plot_image", "mime_type": <string>, "data": <base64>, "is_update": <bool> }`
- Image payload for plot updates.
- If an update is the first image event for a new server request, the server
  treats it as a new response image and includes a server notice that it updates
  the previously sent image.

`session_end`
- `{ "type": "session_end" }`
- Indicates the worker session is terminating.

## Notes

- Output streams (stdout/stderr) remain on the main pipes and are captured separately.
- The server infers request completion when prompt/readline sideband facts show
  that the worker is waiting for the next input.
- To reduce IPC-vs-output capture races, the server applies a short
  post-completion settle window so output reader threads can drain final bytes
  before snapshotting output.
- On timeout, a request may remain pending; later polls can observe the inferred
  completion state and finish the request.
- Backend-specific execution rules should be implemented by the worker. Server
  branching on backend is reserved for interpreter selection, launch setup, tool
  description selection, or narrow capabilities reported at startup.
