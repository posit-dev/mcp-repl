# External Public API Runner

## Summary

- Move public MCP behavior checks, including sandbox-visible real-binary behavior, toward an external Python runner that starts a built `mcp-repl` binary over stdio.
- Keep Rust tests for unit contracts, snapshot normalization, protocol-worker conformance, platform-specific mechanics, and behavior that is not yet covered externally.

## Status

- State: active
- Last updated: 2026-06-18
- Current phase: implementation

## Current Direction

- Grow the minimal Python runner with small, real-client scenarios that speak MCP directly with newline-delimited JSON-RPC.
- Treat sandboxing as product behavior for the external suite. The test runner process is outside the sandbox, but each case starts a built `mcp-repl` binary with an explicit sandbox state and verifies the worker is launched inside that policy through public MCP calls.
- Reintroduce sandbox coverage in the Python runner now, starting with the default `workspace-write` behavior and then adding read-only or full-access contrasts where they prove public behavior.
- Keep each migrated case focused enough that matching Rust integration coverage can be removed or reduced in the same change.
- Use `danger-full-access` only for individual external cases whose purpose is unrelated to sandboxing and where disabling sandbox enforcement does not hide the product behavior under test.
- Keep existing Rust tests discoverable by `cargo test` until their scenario is migrated or removed in the same change that adds equivalent Python coverage.

## Long-Term Direction

- Migrate representative public API integration scenarios out of Rust when the Python runner covers the same real-binary behavior, including sandbox behavior that is observable through public MCP tool calls.
- Keep protocol-worker conformance tests, Rust-only contract tests, and deeply platform-specific sandbox launch mechanics in Rust unless there is a clearer public external scenario for the same contract.

## Phase Status

- Phase 0: completed - add the runner shell and first R console smoke case.
- Phase 1: completed - migrate another small real-client scenario with timeout or busy-worker behavior.
- Phase 2: completed - run the external suite in CI after the debug binary is built.
- Phase 3: active - reintroduced `workspace-write` sandbox behavior in the Python runner, including write policy and network-access policy; continue migrating duplicate real-binary Rust integration coverage case by case.

## Locked Decisions

- The external suite must accept a prebuilt binary path instead of building the binary itself.
- The runner should call MCP tools over stdio and avoid internal Rust helpers.
- CI runs the external suite as its own step after `cargo build` on each matrix target.
- Do not opt Rust test targets out of Cargo discovery in anticipation of future migration work.

## Open Questions

- Which sandbox scenarios have public external equivalents and which should remain Rust-only launch or platform-mechanics coverage.
- Which additional public scenarios should migrate into the external suite before the parent migration is complete.

## Next Safe Slice

- Migrate another representative real-binary Rust integration scenario to the Python runner and remove or reduce only the matching Rust coverage.
- Rename, split, or retire temporary staging suites such as `tests/refactor_coverage.rs` when equivalent scenarios have clearer homes in the external runner or behavior-specific Rust suites.
- Keep additional sandbox migrations focused on public behavior that does not require internal server or launch-state inspection.

## Stop Conditions

- Stop if a migrated scenario requires internal server state inspection instead of public MCP requests.
- Stop if runner behavior needs platform-specific process supervision beyond the simple stdio client.

## Decision Log

- 2026-05-17: Chose a narrow first slice with one R `repl` smoke case to prove the runner can initialize the real binary and call public tools before moving more complex scenarios.
- 2026-05-17: Added an R timeout/busy/recovery case to the external runner and removed the matching Rust snapshot smoke test.
- 2026-05-17: Added an R `repl_reset` state-clearing case to the external runner and removed the duplicate Rust public surface test.
- 2026-05-17: Added the external public API suite to the cross-platform CI workflow as a separate post-build step.
- 2026-05-17: Added an R interrupt/restart-prefix scenario with explicit interrupt readiness polling and removed duplicate Rust prefix tests.
- 2026-05-17: Added files-mode output-bundle scenarios for text bundles, pruning, timeout backfill, and size-cap omission, then removed duplicate broad Rust integration coverage.
- 2026-05-17: Removed obsolete serial scheduling after verifying the remaining Rust REPL binaries pass under normal Cargo test scheduling.
- 2026-05-18: Reaffirmed that unmigrated Rust scenarios must remain discoverable by `cargo test`; migrations should replace Rust coverage with equivalent Python coverage in the same change, not disable tests ahead of time.
- 2026-05-18: Clarified that the external runner itself is not sandboxed, but the spawned `mcp-repl` binary still owns the sandbox contract; the next slice should restore sandbox coverage in the Python runner starting with `workspace-write`.
- 2026-06-18: Added an external `workspace-write` sandbox case that verifies public `repl` behavior for writes inside the server cwd and blocked writes outside it.
- 2026-06-18: Migrated the public read-only workspace write denial check into the external runner and removed the duplicate Rust integration case.
- 2026-06-18: Migrated the public full-access outside-write check into the external runner and removed the duplicate Rust integration case.
- 2026-06-18: Migrated workspace-write network block/allow coverage into the external runner and removed the duplicate Unix Rust integration cases.
- 2026-06-18: Migrated the basic Python console smoke check into the external runner and removed the duplicate Rust integration case.
- 2026-06-18: Migrated the Python busy-input discard check into the external runner and removed the duplicate Rust integration case.
- 2026-06-18: Migrated the R `write_stdin` multiple-call, timeout-polling, error-recovery, huge assignment-input, and files-mode output-spill cases into the external runner and removed the duplicate Rust integration cases.
