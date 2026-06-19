# Windows Test Parity

## Summary

- Remove unnecessary Windows-only skips from public integration tests.
- Run the Windows Rust suite with the same Cargo scheduling as Linux and macOS.
- Keep platform-specific tests only where they cover platform-specific behavior.

## Status

- State: active
- Last updated: 2026-06-19
- Current phase: validation

## Current Direction

- Public end-to-end Rust tests now run on Windows where the product behavior is expected to match other platforms.
- Use normal Windows `cargo test` failures to identify real shared-state, host-dependency, or Windows launcher issues instead of preserving CI serialization as a workaround.
- Keep Windows-specific compatibility tests when they assert Windows-only process, console, sandbox, or path behavior.
- Windows public network-policy cases now require `mcp-repl windows-sandbox setup`
  before the suite so the offline account and firewall rules are present.

## Long-Term Direction

- CI should run the public Python API suite and Rust test suite across Linux, macOS, and Windows without a Windows-only skip or serial lane.
- Any remaining platform difference should have an explicit product reason and matching documentation.

## Phase Status

- Phase 0: completed - inventoried skipped Windows tests and CI serialization.
- Phase 1: completed - enabled skipped public R/Python tests where feasible.
- Phase 2: completed - removed Windows-only CI serialization after local validation.
- Phase 3: completed - Windows managed-network enforcement closes the remaining external public API network skips.

## Locked Decisions

- Public end-to-end integration tests should exercise the same MCP surface across platforms unless the tested behavior is genuinely platform-specific.
- Cargo discovery should remain the source of truth for Rust integration tests.
- Windows test target names must avoid installer/update keywords that trigger UAC installer detection for test executables.
- Standalone ConPTY terminal mode toggles are snapshot noise and should be normalized out of shared snapshots.
- Windows no-network and managed-domain sandbox tests depend on the explicit
  offline-account setup command rather than silently weakening enforcement.

## Open Questions

- Whether optional reticulate help coverage should gain a Windows-specific server fix for hosts where reticulate initializes under `Rscript` but hangs inside the MCP R worker.
- Whether the shared Windows suite server startup mutex is still necessary now that live test sessions pass under ordinary Cargo scheduling.

## Next Safe Slice

- Investigate the reticulate Windows MCP-worker initialization hang separately from test scheduling.

## Stop Conditions

- Stop and ask if a failing Windows test exposes an intended product-contract difference rather than a test harness gap.
- Stop if making Windows parallel requires changing public server semantics instead of isolating test state.

## Decision Log

- 2026-06-18: Started a dedicated plan because this work spans test harness behavior, CI scheduling, docs, and platform parity.
- 2026-06-18: Removed broad Windows compile gates from public R transcript, pager, batch-input, session-ending, server-smoke, and refactor coverage tests after they passed on Windows.
- 2026-06-18: Enabled Python snapshot/help/reset prompt coverage on Windows; kept only raw Unix byte-stream ownership tests Unix-only because Windows ConPTY renders through the console boundary.
- 2026-06-18: Removed Windows-only CI serialization after `cargo test --quiet` passed locally under normal Cargo scheduling.
- 2026-06-18: Renamed Rust test targets containing `install` or `updates` because Windows UAC installer detection can refuse to launch those test executables with `os error 740`.
- 2026-06-18: Kept external public API network policy cases as unsupported on Windows because managed/domain network enforcement is not implemented there yet.
- 2026-06-19: Added Windows sandbox setup to CI and removed the Windows skips
  from the external public API workspace-write network allow/block cases.
