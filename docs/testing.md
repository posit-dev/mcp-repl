# Testing

`mcp-repl` is validated primarily through public API tests and transcript-style snapshots.
This file is the entrypoint for deciding how to verify a change.

## Core Test Surface

- `tests/run_integration_tests.py`: external real-binary checks over MCP stdio, including basic R `repl`, pager command handling, files-mode output bundles, timeout/busy recovery, interrupt/restart prefixes, and `repl_reset` state clearing.
- `tests/common/`: shared Rust MCP harness for public tool calls, transcript snapshots, sandbox assertions, and client-install fixtures.
- `tests/repl_surface.rs`, `tests/server_smoke.rs`, `tests/mcp_transcripts.rs`, and `tests/write_stdin_*.rs`: core `repl`/`repl_reset` behavior, timeout polling, oversized text replies, transcript-file behavior, and snapshot coverage through the public tool API.
- `tests/pager*.rs` and `tests/oversized_output_cli.rs`: pager mode, files mode, and oversized-output CLI behavior.
- `tests/python_*.rs`, `tests/r_*.rs`, `tests/plot_images.rs`, and `tests/python_plot_images.rs`: backend-specific public behavior, help/manual surfaces, PTY-backed Python readline behavior, and image output.
- `tests/zod_protocol.rs`: protocol-worker conformance, including PTY launch with sideband IPC kept separate from visible PTY output.
- `tests/sandbox.rs` and `tests/sandbox_state_updates.rs`: sandbox policy behavior and Codex per-tool-call sandbox metadata.
- `tests/install_*.rs`, `tests/codex_approvals_tui.rs`, and `tests/claude_integration.rs`: install-path and real client integration coverage.
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
uses `--sandbox danger-full-access` by default so the suite stays focused on
client protocol behavior rather than sandbox policy.

Use `--case <name>` to run one public API case while iterating.

CI runs this suite after `cargo build` in the main cross-platform workflow,
using the debug binary built for each matrix target.

## Fast Quiet Rust Suite

Use this while iterating on ordinary Rust tests locally:

```sh
cargo nextest run --show-progress none
```

The checked-in `.config/nextest.toml` default profile keeps passing-test output
quiet and shows failure output in the final report. It opts the interrupt
binary into a one-at-a-time group because those tests coordinate through
process-local fixtures. The default local profile includes real client
integration binaries. Use this when Codex and Claude are installed and
authenticated locally.

The `--show-progress none` flag hides progress output so successful runs stay
compact in local terminals and CI logs; nextest treats that as user
configuration rather than a repository profile key.

The CI workflow uses the CI nextest profile for the ordinary Rust suite after
`cargo clippy`, with `--profile ci --show-progress none` on the command line.
The CI profile excludes real client integration binaries from the ordinary Rust
suite. CI installs Codex and runs `codex_approvals_tui` separately against a
mocked model provider. Windows keeps the ordinary suite fully serial with
`--build-jobs 1` and `--test-threads 1`.

Plain `cargo test` remains the full Cargo compatibility path. It must continue
to discover the binary unit tests and Rust integration targets. Do not opt Rust
test targets out of Cargo discovery in anticipation of a future Python
migration; migrate a scenario only when the Rust coverage is deleted or reduced
in the same change that adds equivalent external coverage.

## Real Client Integrations

CI installs Codex and runs the Codex integration binary. The Codex CI
integration does not require OpenAI authentication because the test config
points Codex at a local mock provider.

By default, the Codex integration uses `MCP_REPL_CODEX_BACKEND=auto`: it checks
whether Codex is logged in, checks whether `gpt-5.3-codex-spark` is available,
and uses that live backend when both checks pass. Otherwise it uses the mocked
provider. Set `MCP_REPL_CODEX_BACKEND=live` or `MCP_REPL_CODEX_BACKEND=mock`
to force one path.

When changing Codex backend selection or CI real-client wiring, run the forced
mock path explicitly:

```sh
MCP_REPL_CODEX_BACKEND=mock cargo test -j 1 --test codex_approvals_tui codex_exec_auto_backend_smoke -- --test-threads=1
```

To validate the authenticated live path directly on a machine with Spark access:

```sh
MCP_REPL_CODEX_BACKEND=live cargo test -j 1 --test codex_approvals_tui codex_exec_auto_backend_smoke -- --test-threads=1
```

Local full verification includes the Codex and Claude integration binaries when
those clients are installed. Codex uses the Spark model
(`gpt-5.3-codex-spark`) in its isolated test config. Claude uses `haiku`.
If a required client binary is unavailable, the matching integration test prints
a skip banner with the reason. Codex backend selection prints a `CODEX` banner
showing whether the test selected live Spark or the mocked provider.

To run only those integrations:

```sh
cargo nextest run --show-progress none --test codex_approvals_tui --test claude_integration
```

CI runs the Codex integration binary; Claude integration remains local because
provider authentication is unavailable in CI.

## Full Verification Before Replying

If you modify code, run:

- `cargo check`
- `cargo build`
- `python3 tests/run_integration_tests.py --binary target/debug/mcp-repl`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo nextest run --show-progress none`
- `cargo test`
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
