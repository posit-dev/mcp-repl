# IPC Module Split

## Summary

- Split `src/ipc.rs` into protocol, server connection, worker connection, emit, transport, and test support modules.
- Kept `src/ipc.rs` as the facade so existing `crate::ipc::*` imports continue to work.
- Preserved protocol JSON, IPC environment names, timeout behavior, prompt history, image IDs, PTY validation, worker-ready validation, fork IPC behavior, and Windows named-pipe behavior.

## Status

- State: completed
- Last updated: 2026-06-18
- Current phase: completed

## Current Direction

- The IPC implementation now lives in focused child modules under `src/ipc/`.
- The facade re-exports the existing public IPC surface.
- Tests were moved with the code they validate.

## Phase Status

- Phase 0: completed - split code and imports.
- Phase 1: completed - ran required checks and resolved mechanical module issues.
- Phase 2: completed - moved this plan to completed.

## Locked Decisions

- `src/ipc.rs` remains a facade with `pub use` re-exports.
- No call-site churn outside IPC was needed.
- No protocol or runtime behavior changes were made.

## Open Questions

- None.

## Verification Notes

- `cargo check` passed.
- `cargo build` passed.
- `python3 tests/run_integration_tests.py --binary target/debug/mcp-repl` passed when rerun outside the managed sandbox after `sandbox-exec` was denied in the sandboxed attempt.
- `cargo clippy --all-targets --all-features -- -D warnings` passed.
- `cargo test --quiet` passed after rerunning a transient R `parallel` load failure.
- `cargo +nightly fmt` passed.

## Decision Log

- 2026-06-18: Started the module split as a behavior-preserving refactor with the existing public IPC facade.
- 2026-06-18: Completed the split and moved the plan to completed.
