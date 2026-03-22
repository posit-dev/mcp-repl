# Architecture

`mcp-repl` is a single Rust binary that exposes a long-lived REPL runtime over MCP stdio.
The repository is organized around a few concrete subsystems rather than deep package layering.

## Subsystem Map

### CLI and install path

- `src/main.rs` parses CLI flags, chooses the backend, and dispatches to server, worker, debug REPL, or install mode.
- `src/install.rs` writes client configuration for Codex and Claude and keeps sandbox-related install defaults consistent.

### Server and request lifecycle

- `src/server.rs` owns the MCP surface, request handling, timeout model, and worker lifecycle.
- `src/server/timeouts.rs` and `src/server/response.rs` keep the public `repl`/`repl_reset` behavior stable.

### Worker and backends

- `src/worker.rs`, `src/worker_process.rs`, and `src/worker_protocol.rs` manage the child runtime and the server-to-worker contract.
- `src/backend.rs` selects between the R and Python implementations.
- R-specific behavior lives in `src/r_session.rs`, `src/r_controls.rs`, `src/r_graphics.rs`, and `src/r_htmd.rs`.
- Python startup is driven by the worker plus the files under `python/`.

### Sandbox and process isolation

- `src/sandbox.rs`, `src/sandbox_cli.rs`, and `src/windows_sandbox.rs` implement OS-level sandboxing, writable-root policy, and client-driven sandbox updates.
- The sideband and sandbox contracts are documented in `docs/sandbox.md` and `docs/worker_sideband_protocol.md`.

### Output, images, and debug surfaces

- `src/pending_output_tape.rs`, `src/output_stream.rs`, and the optional pager modules under `src/pager/` shape tool output.
- `src/debug_logs.rs`, `src/event_log.rs`, and `src/debug_repl.rs` make the runtime legible to agents and humans during investigation.

### Validation harnesses

- `tests/` is the primary public validation surface. The tests exercise tool behavior, snapshots, sandboxing, and client integrations through the exposed MCP interface.

## Design Constraints

- The happy path is a stateful REPL session that persists across tool calls.
- Sandboxing is part of the product contract, not an optional wrapper.
- Tests should target public behavior. Internal helpers are there to support the public REPL surface, not to become separate products.
