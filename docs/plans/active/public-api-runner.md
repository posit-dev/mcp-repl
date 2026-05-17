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
- Phase 2: pending - decide how the external suite should run in CI.

## Locked Decisions

- The external suite must accept a prebuilt binary path instead of building the binary itself.
- The runner should call MCP tools over stdio and avoid internal Rust helpers.

## Open Questions

- Which existing integration file should provide the next migrated scenario.
- Whether CI should run the Python suite as a separate required step or as part of a broader fast test profile.

## Next Safe Slice

- Decide whether the external suite should run in CI now, or migrate one more small real-client scenario before making it required.

## Stop Conditions

- Stop if a migrated scenario requires internal server state inspection instead of public MCP requests.
- Stop if runner behavior needs platform-specific process supervision beyond the simple stdio client.

## Decision Log

- 2026-05-17: Chose a narrow first slice with one R `repl` smoke case to prove the runner can initialize the real binary and call public tools before moving more complex scenarios.
- 2026-05-17: Added an R timeout/busy/recovery case to the external runner and removed the matching Rust snapshot smoke test.
