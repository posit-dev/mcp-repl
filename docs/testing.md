# Testing

`mcp-repl` is validated primarily through public API tests and transcript-style snapshots.
This file is the entrypoint for deciding how to verify a change.

## Core Test Surface

- [`tests/repl_surface.rs`](../tests/repl_surface.rs): basic `repl` and `repl_reset` behavior.
- [`tests/server_smoke.rs`](../tests/server_smoke.rs): end-to-end MCP session smoke coverage.
- [`tests/sandbox.rs`](../tests/sandbox.rs) and [`tests/sandbox_state_updates.rs`](../tests/sandbox_state_updates.rs): sandbox policy behavior and client-driven updates.
- [`tests/plot_images.rs`](../tests/plot_images.rs) and [`tests/python_plot_images.rs`](../tests/python_plot_images.rs): plot/image behavior through the public tool surface.
- [`tests/codex_approvals_tui.rs`](../tests/codex_approvals_tui.rs) and [`tests/claude_integration.rs`](../tests/claude_integration.rs): client integration coverage.

## Snapshot Workflow

- Transcript and JSON snapshots live under [`tests/snapshots/`](../tests/snapshots).
- Preferred loop:
  - `cargo insta test`
  - `cargo insta pending-snapshots`
  - `cargo insta review` or `cargo insta accept` / `cargo insta reject`
- Do not delete `tests/snapshots/*.snap.new` manually. Use `cargo insta reject`.

## Full Verification Before Replying

If you modify code, run:

- `cargo check`
- `cargo build`
- `cargo clippy`
- `cargo test`
- `cargo +nightly fmt`

## Debug-Then-Validate Loop

When behavior is unclear:

1. Reproduce through the public tool surface or an existing integration test.
2. Inspect with [docs/debugging.md](debugging.md):
   - `MCP_REPL_DEBUG_DIR`
   - `--debug-repl`
   - the stdio trace proxy
3. Add or update a public API test.
4. Re-run the full verification set.

## Eval Harnesses

- Local `eval/tasks/` and `eval/inspect_ai/` directories may exist for experimental agent benchmarks.
- Treat them as optional feedback loops, not tracked release-critical test inputs.

These evals help measure agent usefulness when present, but they do not replace the integration test suite under [`tests/`](../tests).
