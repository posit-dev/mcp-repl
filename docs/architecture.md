# Architecture

`mcp-repl` is a single Rust binary that exposes a long-lived REPL runtime over MCP stdio.
The repository is organized around a few concrete subsystems rather than deep package layering.

## Subsystem Map

### CLI and install path

- `src/main.rs` parses CLI flags, chooses the backend, and dispatches to server, worker, debug REPL, or install mode.
- `src/install.rs` writes client configuration for Codex and Claude and keeps sandbox-related install defaults consistent, including pinning `--oversized-output files` in installed configs even though bare `mcp-repl` defaults to pager.

### Server and request lifecycle

- `src/server.rs` owns the MCP surface, request handling, timeout model, and worker lifecycle.
- `src/server/timeouts.rs` and `src/server/response.rs` keep the public `repl`/`repl_reset` behavior stable.
- During steady-state requests, the server should treat the worker as a generic
  runtime endpoint: stdin carries accepted input, `output_text` sideband frames
  carry worker-owned text, raw stdout/stderr carry unowned visible text, and
  other sideband events carry structural facts. Backend-specific runtime
  semantics belong in the worker or in explicitly advertised worker metadata.
- Control-only interrupts are routed to an existing worker process without
  interpreting prompt text. Prompt text is display data, so it must not decide
  whether Ctrl-C reaches the runtime.

### Worker and backends

- `src/worker.rs`, `src/worker_process.rs`, and `src/worker_protocol.rs` manage the child runtime and the server-to-worker contract.
- `src/backend.rs` selects between the R and Python implementations at launch
  and install/configuration boundaries.
- Worker launch chooses the runtime stdin transport up front. R and the default
  protocol-worker path use pipes; built-in Unix Python uses PTY-backed C
  stdin/stdout/stderr so CPython takes its normal interactive readline path.
- Both backends receive request payloads through worker stdin and use sideband
  IPC for structured facts. R owns stdin through a worker reader thread keyed by
  payload byte length. Unix Python lets CPython own stdin through
  `PyOS_ReadlineFunctionPointer`; the callback reports `readline_start`,
  `readline_input`, and `readline_discard` accounting facts. Its legacy
  `stdin_write_ack` frames acknowledge request-boundary setup, not prompt
  completion or output delivery.
- The IPC sideband is single-owner by design: startup env vars only bootstrap the main worker, then they are scrubbed before user code runs. Descendants must not emit sideband messages.
- R-specific behavior lives in `src/r_session.rs`, `src/r_controls.rs`, `src/r_graphics.rs`, and `src/r_htmd.rs`.
- Python-specific behavior lives in `src/python_ffi.rs`, `src/python_session.rs`, `src/python_worker.rs`, and `python/embedded.py`. Python worker mode dynamically loads CPython only after the worker has selected the Python backend, so R worker mode does not load Python. On the Unix PTY path, Python leaves CPython's fd-backed stdin surface intact; direct fd stdin consumers are not a request-completion contract.

### Sandbox and process isolation

- `src/sandbox.rs`, `src/sandbox_cli.rs`, and `src/windows_sandbox.rs` implement OS-level sandboxing, writable-root policy, and Codex per-tool-call sandbox metadata handling.
- The sideband and sandbox contracts are documented in `docs/sandbox.md` and `docs/worker_sideband_protocol.md`.

### Output, images, and debug surfaces

- `src/pending_output_tape.rs` and `src/output_stream.rs` stage worker text and images until reply sealing.
- `docs/output_timeline.md` describes how the server reconstructs one visible timeline from stdout/stderr capture plus sideband IPC, and how request completion only gates final-reply cleanup rather than ordering.
- PTY-backed workers may expose one raw terminal output stream rather than
  independent raw stdout and stderr pipes. Worker-owned `output_text` frames
  preserve their declared stream, but raw PTY output can have terminal effects
  such as CRLF translation, echo, terminal-width behavior, and merged stream
  identity.
- `src/server/response.rs` is the server-owned response finalizer. It separates worker-originated text from server-only notices, creates oversized-output bundle directories with lazily materialized `transcript.txt`, `events.log`, and `images/`, applies bundle retention and cleanup policy, and decides the bounded inline preview at seal time.
- `src/pager/` implements the pager-mode oversized-output path used by bare CLI defaults and explicit `--oversized-output pager` installs.
- Longer-term output follow-ons such as per-turn history bundles and a unified resolved-timeline pipeline live in `docs/futurework/per-turn-history-bundles.md` and `docs/futurework/unified-output-timeline-pipeline.md`.
- `src/debug_logs.rs`, `src/event_log.rs`, and `src/debug_repl.rs` make the runtime legible to agents and humans during investigation.

### Validation harnesses

- `tests/run_integration_tests.py` starts an already-built `mcp-repl` binary and
  exercises public MCP tools over stdio. It covers representative real-binary
  behavior that should not depend on Rust internals.
- `tests/` contains the Rust public API, snapshot, sandbox, backend, install,
  protocol-worker, and client-integration suites. Most tests exercise behavior
  through the exposed MCP interface using the shared harness in `tests/common/`.
- `.config/nextest.toml` defines the quiet local Rust suite and a CI-filtered
  ordinary Rust suite. CI runs the Codex integration separately after installing
  the real Codex CLI. The tests should not depend on special local scheduling.

## Design Constraints

- The happy path is a stateful REPL session that persists across tool calls.
- Sandboxing is part of the product contract, not an optional wrapper.
- Tests should target public behavior. Internal helpers are there to support the public REPL surface, not to become separate products.
