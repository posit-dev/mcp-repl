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
- During steady-state worker requests, the server treats the worker as an opaque
  queued runtime endpoint: `input_batch` carries accepted input over IPC,
  `output_text` and `output_image` sideband frames carry worker-owned output,
  raw stdout/stderr carry unowned visible text, and other sideband events carry
  structural facts.
  Backend-specific runtime semantics belong in the worker or in explicitly
  advertised worker metadata.
- Control-only interrupts are routed to an existing worker process without
  interpreting prompt text. Prompt text is display data, so it must not decide
  whether Ctrl-C reaches the runtime.

### Worker and backends

- `src/worker.rs`, `src/worker_process.rs`, and `src/worker_protocol.rs` manage the child runtime and the server-to-worker contract.
- `src/backend.rs` selects between the R and Python implementations at launch
  and install/configuration boundaries.
- Worker launch chooses the raw process stdio or PTY transport up front, but
  accepted `repl` input is queued through IPC during steady-state execution.
  Runtime stdin surfaces are worker-owned implementation details.
- On Windows, Python workers may use ConPTY as their raw terminal envelope.
  Sideband named pipes still carry accepted input, readiness, and worker-owned
  output facts separately from ConPTY traffic.
- Workers receive request payloads through `input_batch` and complete an input
  batch with `input_wait`, `ready`, or `session_end`. Follow-up input after
  `input_wait` or `ready` starts a fresh `input_batch`; the runtime decides
  where it is consumed.
- After `worker_ready`, the worker is not ready for input until its first
  `input_wait` or `ready`. The server treats these as readiness gates, not as
  prompt classification.
- Worker reset and teardown use the sideband `shutdown` lifecycle message first,
  with stdin close and process termination retained only as bounded fallbacks.
- The IPC sideband is single-owner by design: startup env vars only bootstrap the main worker, then they are scrubbed before user code runs. Descendants must not emit sideband messages.
- R-specific behavior lives in `src/r_session.rs`, `src/r_controls.rs`, `src/r_graphics.rs`, and `src/r_htmd.rs`.
- Python-specific behavior lives in `src/python_ffi.rs`, `src/python_session.rs`, `src/python_worker.rs`, and `python/embedded.py`. Python worker mode dynamically loads CPython only after the worker has selected the Python backend, so R worker mode does not load Python. On Unix, Python may still use PTY-backed process stdio for terminal behavior, but managed input batches are served from the worker queue; direct stdin consumers are not a server completion contract.

### Sandbox and process isolation

- `src/sandbox.rs`, `src/sandbox_cli.rs`, and `src/windows_sandbox.rs` implement OS-level sandboxing, writable-root policy, and Codex per-tool-call sandbox metadata handling.
- The sideband and sandbox contracts are documented in `docs/sandbox.md` and `docs/worker_sideband_protocol.md`.

### Output, images, and debug surfaces

- `src/pending_output_tape.rs` and `src/output_stream.rs` stage worker text and images until reply sealing.
- `docs/output_timeline.md` describes how the server reconstructs one visible timeline from stdout/stderr capture plus sideband IPC, and how request completion only gates final-reply presentation rather than ordering.
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
  custom worker protocol, and client-integration suites. Most tests exercise behavior
  through the exposed MCP interface using the shared harness in `tests/common/`.
- CI uses Cargo's standard Rust test runner after installing the real Codex CLI,
  with the Codex backend forced to the mocked provider. The tests should not
  depend on special local scheduling.

## Design Constraints

- The happy path is a stateful REPL session that persists across tool calls.
- Sandboxing is part of the product contract, not an optional wrapper.
- Tests should target public behavior. Internal helpers are there to support the public REPL surface, not to become separate products.
