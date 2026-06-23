# Linux Permission-Profile Sandbox

## Summary

- Rework `mcp-repl` sandbox internals so Linux behavior uses permission-profile metadata and a helper-backed enforcement path.
- Preserve the CLI arguments `mcp-repl` already accepts (`--sandbox`, `--add-writable-root`, `--add-allowed-domain`, and supported `--config` keys), but treat old `read-only`, `workspace-write`, and `danger-full-access` modes as inputs that compile into permission profiles.
- Optimize for Linux first. macOS and Windows compatibility shims may remain behind existing cfg boundaries, but they should not constrain the Linux design.

## Status

- State: completed
- Last updated: 2026-06-23
- Current phase: complete

## Current Direction

- Replace the internal Linux authority model with `PermissionProfile`, `FileSystemSandboxPolicy`, `FileSystemSandboxEntry`, `FileSystemPath`, and `NetworkSandboxPolicy`.
- Parse current per-tool-call sandbox metadata by accepting `permissionProfile` as canonical. Legacy `sandboxPolicy` may remain only as a minimal compatibility input and should immediately compile into a permission profile.
- Launch Linux workers through a `codex-linux-sandbox` arg0-compatible helper surface that accepts `--permission-profile`, applies bubblewrap by default, applies seccomp after bubblewrap, and keeps legacy Landlock only as an explicit fallback knob.

## Long-Term Direction

- The durable sandbox contract should be permission-profile based across CLI, MCP metadata, tests, and docs.
- The legacy `SandboxPolicy` enum should become a thin CLI/config compatibility adapter, not the shape used for Linux enforcement or inherited metadata.
- Claude Code support should map Claude sandbox settings into the same permission-profile model rather than adding another runtime policy representation.

## Phase Status

- Phase 0: completed - inspected the existing `mcp-repl` sandbox implementation.
- Phase 1: completed - introduced permission-profile policy types and updated Linux launch/metadata translation.
- Phase 2: completed - revised focused tests and docs around the new policy shape.
- Phase 3: completed - ran required checks and fixed fallout.

## Locked Decisions

- `permissionProfile` is the canonical inherited metadata field.
- Missing inherited metadata remains fail-closed for `--sandbox inherit`.
- `mcp-repl` keeps its existing CLI arguments, but backwards compatibility for older internal JSON/log/snapshot shapes is not a goal.
- Linux filesystem isolation should prefer bubblewrap; Landlock-only filesystem enforcement is legacy fallback behavior.
- The session temp directory remains server-owned and is added to the effective writable set for worker runtime needs.
- `useLegacyLandlock=false` from inherited metadata is not a local bubblewrap override; only `true` forces the legacy path.
- Restricted-read profiles require the bubblewrap backend for exact read enforcement. Legacy Landlock fallback broadens reads while still enforcing writable roots.
- When running inside a restricted parent sandbox environment, default Linux startup avoids spawning a trial `bwrap` probe and starts with the legacy fallback path unless `MCP_REPL_USE_LINUX_BWRAP=1` explicitly requests bubblewrap.

## Open Questions

- How much Claude sandbox configuration should be mapped in this change versus documented as follow-up after this Linux sandbox change lands.
- Whether `mcp-repl` should later adopt protected-create handling for missing `.git`, `.codex`, and `.agents` paths under bubblewrap writable roots.

## Completion Notes

- Implemented the Linux permission-profile runtime model and helper launch path.
- Updated sandbox metadata tests, docs, and verification coverage.
- Completed the repository verification required by `AGENTS.md`.

## Stop Conditions

- Stop and ask before broadening the task into non-Linux sandbox behavior changes.
- Stop if an inherited metadata or helper semantic cannot be represented without changing the public `repl`/`repl_reset` MCP API.

## Decision Log

- 2026-06-23: Chose permission profiles as the internal policy model because current metadata and Linux helper code both use that shape directly.
- 2026-06-23: Chose a Linux-first implementation because the user explicitly scoped behavior to Linux for this task.
- 2026-06-23: Kept the existing CLI argument surface as a compatibility adapter and made `permissionProfile` the Linux enforcement shape.
- 2026-06-23: Kept a legacy Landlock fallback for hosts where bubblewrap cannot start, including restricted test sandboxes that cannot create nested user namespaces.
