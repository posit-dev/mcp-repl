# Testing

`mcp-repl` is validated primarily through public API tests and transcript-style snapshots.
This file is the entrypoint for deciding how to verify a change.

## Core Test Surface

- `tests/run_integration_tests.py`: external real-binary checks over MCP stdio, including basic R `repl`, pager command handling, files-mode output bundles, timeout/busy recovery, interrupt/restart prefixes, `repl_reset` state clearing, and public sandbox policy behavior.
- `tests/common/`: shared Rust MCP harness for public tool calls, transcript snapshots, sandbox assertions, and client-install fixtures.
- `tests/repl_surface.rs`, `tests/server_smoke.rs`, `tests/mcp_transcripts.rs`, and `tests/write_stdin_*.rs`: core `repl`/`repl_reset` behavior, timeout polling, oversized text replies, transcript-file behavior, and snapshot coverage through the public tool API.
- `tests/pager*.rs` and `tests/oversized_output_cli.rs`: pager mode, files mode, and oversized-output CLI behavior.
- `tests/python_*.rs`, `tests/r_*.rs`, `tests/plot_images.rs`, and `tests/python_plot_images.rs`: backend-specific public behavior, help/manual surfaces, queued stdin/readline behavior, and image output.
- `tests/zod_protocol.rs`: IPC-queued custom worker conformance, including PTY launch with sideband IPC kept separate from visible PTY output.
- `tests/sandbox.rs` and `tests/sandbox_state_meta.rs`: sandbox policy behavior and Codex per-tool-call sandbox metadata.
- `tests/client_config_dual_backend.rs`, `tests/release_script.rs`, `tests/codex_integration.rs`, and `tests/claude_integration.rs`: install-path and real client integration coverage.
- `tests/docs_contracts.rs`: docs map and snapshot-facing documentation contracts.

## Snapshot Workflow

- Transcript and JSON snapshots live under `tests/snapshots/`.
- Preferred loop:
  - `cargo insta test`
  - `cargo insta pending-snapshots`
  - `cargo insta review` or `cargo insta accept` / `cargo insta reject`
- CI-style validation: `cargo insta test --check`
- Do not add `--unreferenced=reject` to the general snapshot check; this
  repository keeps valid platform-specific snapshots that are unreferenced on
  other platforms.
- Do not delete `tests/snapshots/*.snap.new` manually. Use `cargo insta reject`.

## External Public API Suite

Build the binary first, then run the Python suite:

```sh
cargo build
python3 tests/run_integration_tests.py --binary target/debug/mcp-repl
```

The runner starts the real server over MCP stdio and calls public tools only. It
uses `--sandbox danger-full-access` by default so most cases stay focused on
client protocol behavior rather than sandbox policy. Individual sandbox cases
override that default in their case-specific server args.

Use `--case <name>` to run one public API case while iterating.

CI runs this suite after `cargo build` in the main cross-platform workflow,
using the debug binary built for each matrix target.

## Rust Suite

Use Cargo's standard Rust test runner:

```sh
cargo test
```

The Rust suite uses plain `cargo test` as its single runner. Plain `cargo test`
remains the full Cargo compatibility path. It must continue to discover the
binary unit tests and Rust integration targets. CI passes Cargo's `--quiet`
flag to keep successful logs compact.

```sh
cargo test --quiet
```

CI installs Codex before `cargo test` and sets `MCP_REPL_CODEX_BACKEND=mock`,
so the Codex integration target runs through the mocked provider as part of the
ordinary Rust suite. CI uses the same Cargo scheduling on Linux, macOS, and
Windows by running `cargo test --quiet` for every matrix target.

Do not opt Rust test targets out of Cargo discovery in anticipation of a future
Python migration; migrate a scenario only when the Rust coverage is deleted or
reduced in the same change that adds equivalent external coverage.

## Real Client Integrations

CI installs Codex before the Rust suite. The Codex CI integration does not
require OpenAI authentication because the test config points Codex at a local
mock provider.

By default, the Codex integration uses `MCP_REPL_CODEX_BACKEND=auto`: it checks
whether Codex is logged in, checks whether `gpt-5.3-codex-spark` is available,
and uses that live backend when both checks pass. Otherwise it uses the mocked
provider. Set `MCP_REPL_CODEX_BACKEND=live` or `MCP_REPL_CODEX_BACKEND=mock`
to force one path.

When changing Codex backend selection or CI real-client wiring, run the forced
mock path explicitly:

```sh
MCP_REPL_CODEX_BACKEND=mock cargo test -j 1 --test codex_integration codex_exec_auto_backend_smoke -- --test-threads=1
```

To validate the authenticated live path directly on a machine with Spark access:

```sh
MCP_REPL_CODEX_BACKEND=live cargo test -j 1 --test codex_integration codex_exec_auto_backend_smoke -- --test-threads=1
```

Local full verification includes the Codex and Claude integration binaries when
those clients are installed. Codex uses the Spark model
(`gpt-5.3-codex-spark`) in its isolated test config. Claude uses `haiku`.
If a required client binary is unavailable, the matching integration test prints
a skip banner with the reason. Codex backend selection prints a `CODEX` banner
showing whether the test selected live Spark or the mocked provider.

To run only those integrations:

```sh
cargo test --quiet --test codex_integration --test claude_integration
```

CI runs the Codex integration target as part of `cargo test`; Claude integration
remains local because provider authentication is unavailable in CI.

## Full Verification Before Replying

If you modify code, run:

- `cargo check`
- `cargo build`
- `python3 tests/run_integration_tests.py --binary target/debug/mcp-repl`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --quiet`
- `cargo +nightly fmt`

For docs-only changes, run the narrow validation that covers the edited docs.
For agent-facing docs, that is usually:

```sh
cargo test --test docs_contracts
```

## Debug-Then-Validate Loop

When behavior is unclear:

1. Reproduce through the public tool surface or an existing integration test.
2. Inspect with `docs/debugging.md`:
   - `MCP_REPL_DEBUG_DIR`
   - `--debug-repl`
   - the stdio trace proxy
3. Add or update a public API test.
4. Re-run the full verification set.
