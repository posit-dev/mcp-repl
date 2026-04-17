# Codex Sandbox State Meta Migration

## Summary

- Migrate `mcp-repl`'s Codex `--sandbox inherit` integration from the obsolete async sandbox update protocol to Codex's current per-tool-call `_meta["codex/sandbox-state-meta"]` contract.
- Keep `--sandbox inherit` fail-closed: if current Codex does not provide usable sandbox metadata on a tool call, `mcp-repl` must reject the call instead of falling back to a local default.
- Keep explicit sandbox modes such as `--sandbox read-only` and `--sandbox workspace-write` authoritative; Codex metadata must not override them.
- Do not carry backward compatibility for older Codex releases that still depended on the old update channel.

## Status

- State: completed
- Last updated: 2026-04-17
- Current phase: completed

## Design Intent

- When `mcp-repl` is configured with `--sandbox inherit`, Codex is the source of truth for the sandbox state.
- `mcp-repl` must not guess, substitute, or silently fall back to its own default sandbox when it is expecting Codex to provide one.
- Security is the main constraint: if Codex intended `read-only`, `mcp-repl` must not run with broader permissions just because sandbox information was late, missing, or malformed.
- MCP startup should stay fast. `mcp-repl` should not block initialization waiting for sandbox information that belongs to a later tool call.
- Waiting, if any, belongs only at the point where a tool call needs sandbox information in order to run safely.
- The repo should track the current public Codex contract and exercise the real Codex binary in integration coverage so protocol drift is surfaced quickly.

## Motivation

- The old design assumed Codex would push sandbox state out of band, asynchronously, at session startup.
- That assumption is brittle because it separates sandbox selection from the tool call that actually needs the sandbox.
- When that assumption fails, the failure mode is not just a functional bug. It is a security issue, because the effective sandbox can become broader than what Codex intended.
- The correct shape is request-scoped: if `mcp-repl` is inheriting sandbox policy from Codex, it should only run once it has the sandbox information that applies to that call.
- The simplest safe path is to target current Codex directly instead of layering compatibility logic around an obsolete protocol.

## Current Direction

- Treat the current release of Codex as the only target contract for this slice.
- Replace the server-side sandbox update listener with per-tool-call parsing of `_meta["codex/sandbox-state-meta"]`.
- Advertise only the current Codex experimental capability needed for that metadata path.
- Rebuild the public tests around the new contract before changing runtime code.

## Long-Term Direction

- The long-term contract should be simple: `mcp-repl` determines the inherited sandbox directly from the Codex tool call that is about to execute.
- Startup should stay fast. `mcp-repl` should not block MCP initialization waiting for sandbox state that belongs to a later tool call.
- Any server state retained between calls should be minimal bookkeeping, not a second sandbox synchronization protocol.

## Phase Status

- Phase 0: completed
  - Audited the current `inherit` path, documented the protocol shift, and locked the bounded design.
- Phase 1: completed
  - Added failing public regressions for metadata-driven sandbox inheritance and fail-closed behavior.
- Phase 2: completed
  - Implemented the runtime migration and removed obsolete update-handling code.
- Phase 3: completed
  - Refreshed real-Codex integration coverage, docs, and final verification.

## Locked Decisions

- Do not implement compatibility shims for older Codex sandbox update behavior.
- Do not infer `read-only` versus `workspace-write` from coarse metadata such as `x-codex-turn-metadata.sandbox`; that signal is not precise enough.
- Do not fall back from missing Codex sandbox metadata to `mcp-repl`'s local default policy.
- Do not let Codex metadata override explicit non-`inherit` CLI sandbox modes.
- Prefer a single happy path: current Codex should supply `codex/sandbox-state-meta` on each tool call, and `mcp-repl` should consume that directly.

## Outcome

- `mcp-repl` now advertises `codex/sandbox-state-meta` only when `--sandbox inherit` is configured.
- `repl` and `repl_reset` now derive inherited sandbox state from each tool call’s `_meta["codex/sandbox-state-meta"]`.
- Missing or malformed metadata fails closed with the existing inherit error path.
- Explicit non-`inherit` sandbox modes ignore Codex metadata.
- The old async sandbox update listener and startup settle logic were removed from the active runtime contract.

## Verification

- `cargo check`
- `cargo build`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test`
- `cargo +nightly fmt --all`

## Notes

- Current Codex source and live traces both showed the old async update protocol was obsolete for the current release line.
- The migration stayed intentionally single-path: no compatibility layer for older Codex builds.

## Decision Log

- 2026-04-17: Scoped the work to current-release Codex only. Older async sandbox update behavior is out of scope.
- 2026-04-17: Locked `--sandbox inherit` to remain fail-closed. Missing or malformed Codex sandbox metadata must reject the tool call.
- 2026-04-17: Chose per-tool-call `_meta["codex/sandbox-state-meta"]` as the source of truth after inspecting current Codex source and live traces.
- 2026-04-17: Completed the repo migration and verification against the real current Codex integration tests.
