# Stage 1: Remove Paging and Preserve Raw Output Metadata

## Summary

- Remove server-side paging, truncation notices, synthetic input summaries, and echo elision from `repl`.
- Keep the worker behavior and sideband wire protocol unchanged.
- Replace ring-based reply assembly with a single always-present `PendingOutputTape`.
- Preserve sideband ordering metadata so later stages can make richer echo decisions without changing this stage's visible behavior.

## Architecture

- `PendingOutputTape` is the shared accumulator owned by `WorkerManager`.
- Stdout and stderr reader threads stay dumb: they forward raw bytes into the tape.
- The tape keeps per-stream partial-line buffers and commits ordered `TextFragment`, `Image`, and `Sideband` events.
- The tape assigns a monotonic server-local sequence number to each committed event.
- Sideband markers such as `readline_start`, `readline_result`, `request_end`, and `session_end` are mirrored into the tape for later formatting decisions.

## Rendering Rules

- Final reply rendering drains a `PendingOutputSnapshot`.
- Valid UTF-8 is rendered normally.
- Invalid UTF-8 bytes are rendered inline as `\xNN`.
- Stderr stays visible via `stderr: ` prefixes during final rendering.
- Echoed input is returned verbatim in this stage. No elision, omission, or trimming happens now.
- Image update collapsing remains in the response conversion path.

## Public Behavior Changes

- Large outputs are returned directly in tool responses.
- Empty `input` only polls for pending output.
- `:` commands are no longer intercepted by the server.
- These messages disappear:
  - `[pager] ...`
  - `[repl] echoed input elided ...`
  - `[repl] output truncated ...`
  - `[repl] input: ...`

## Future Echo Work

- The formatter receives raw output plus the server-observed sideband timeline.
- That allows later stages to correlate visible lines with readline events and decide whether a line is echoed input or output that followed echoed input.
- No such filtering is applied in this stage.
