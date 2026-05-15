# Public API Test Suite

## Summary

- Move the default test suite toward public MCP API coverage that launches the built `mcp-repl` binary.
- Prefer compact transcript and JSON snapshots for response behavior.
- Keep direct internal tests only when they document intent that cannot be exercised through a public tool call.

## Status

- State: active
- Last updated: 2026-05-15
- Current phase: implementation

## Current Direction

- Treat `tests/common::McpSnapshot` and `McpTestSession` as the main public API harness.
- Make real Codex and Claude client integrations explicit opt-in checks instead of default local `cargo test` work.
- Remove helper-only tests first, then collapse verbose public API tests into smaller snapshot scenarios.

## Long-Term Direction

- The normal suite should be mostly binary-level MCP tests and snapshots.
- Expensive client integrations, platform probes, and long matrix-style cases should be separate opt-in validation.
- Internal unit tests should be rare and justified by a public contract gap.

## Phase Status

- Phase 0: active - document the policy and remove helper-only drift.
- Phase 1: pending - consolidate long public integration files into smaller snapshot scenarios.
- Phase 2: pending - audit `src/` unit tests and either replace them with public API snapshots or record why they remain.

## Locked Decisions

- Default tests exercise `mcp-repl` as a binary over MCP stdio.
- Snapshots are the preferred assertion format for tool-call transcript behavior.
- Real Codex and Claude client integrations require `MCP_REPL_RUN_CLIENT_INTEGRATIONS=1`.

## Open Questions

- Which internal unit tests remain necessary as intent documentation after public snapshot coverage exists?
- Which slow sandbox and Python backend scenarios can be represented by one or two transcript snapshots instead of many assertion tests?

## Next Safe Slice

- Consolidate long public API integration files into fewer transcript snapshots.
- Add or reuse public API snapshots before deleting behavioral assertions that currently cover real runtime contracts.

## Stop Conditions

- Stop and ask before deleting a test that is the only known coverage for a public behavior.
- Stop if a public API replacement would broaden or change the runtime contract.

## Decision Log

- 2026-05-15: Started the suite cleanup with an opt-in boundary for real client integrations and a policy that defaults to binary-level MCP tests.
- 2026-05-15: Warmed `cargo test` timing did not improve in this slice: baseline `HEAD` took 309.18s and this change took 320.64s. The helper-only deletions reduced test count from 757 to 753, but the long runtime remains in public integration tests such as `python_backend`, `sandbox_state_updates`, and `write_stdin_behavior`.
