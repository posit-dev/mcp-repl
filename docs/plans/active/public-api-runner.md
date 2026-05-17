# External Public API Runner

## Summary

- Move public MCP behavior checks toward an external Python runner that starts a built `mcp-repl` binary over stdio.
- Keep Rust tests for unit contracts, snapshot normalization, platform-specific mechanics, and behavior that is not yet covered externally.

## Status

- State: active
- Last updated: 2026-05-17
- Current phase: implementation

## Current Direction

- Grow the minimal Python runner with small, real-client scenarios that speak MCP directly with newline-delimited JSON-RPC.
- Keep each migrated case focused enough that matching Rust integration coverage can be removed or reduced in the same change.
- Use `--sandbox danger-full-access` by default for the external suite so the early cases test client protocol behavior, not sandbox policy.

## Long-Term Direction

- Migrate representative public API integration scenarios out of Rust when the Python runner covers the same real-binary behavior.
- Keep sandbox-policy tests, protocol-worker conformance tests, and Rust-only contract tests in Rust unless there is a clearer public external scenario.

## Phase Status

- Phase 0: completed - add the runner shell and first R console smoke case.
- Phase 1: completed - migrate another small real-client scenario with timeout or busy-worker behavior.
- Phase 2: completed - run the external suite in CI after the debug binary is built.

## Locked Decisions

- The external suite must accept a prebuilt binary path instead of building the binary itself.
- The runner should call MCP tools over stdio and avoid internal Rust helpers.
- CI runs the external suite as its own step after `cargo build` on each matrix target.

## Open Questions

- Which additional public scenarios should migrate into the external suite before the parent migration is complete.

## Next Safe Slice

- Migrate the next representative public scenario only if it can replace or reduce matching Rust integration coverage in the same change.

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
