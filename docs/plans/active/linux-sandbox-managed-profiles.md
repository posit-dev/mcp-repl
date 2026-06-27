# Linux Sandbox Managed Permission Profiles

## Summary

- Rework the Linux sandbox service around permission-profile driven policy
  resolution, bubblewrap-first filesystem isolation, seccomp for
  network/process restrictions, and legacy Landlock only as an explicit
  fallback.
- Keep the existing `mcp-repl` public sandbox API surface where practical:
  `SandboxPolicy`, per-tool-call sandbox metadata parsing for `--sandbox
  inherit`, session temp directory ownership, and worker restart semantics
  remain server-owned.

## Status

- State: active
- Last updated: 2026-06-26
- Current phase: follow-up scoping

## Current Direction

- Replace the Linux-only legacy projection path with direct enforcement of `FileSystemSandboxPolicy` in the Linux helper.
- Default Linux filesystem sandboxing to bubblewrap when a sandbox is required, with `useLegacyLandlock` and `features.use_linux_sandbox_bwrap=false` acting as the explicit legacy Landlock path.
- Keep using the in-process `mcp-repl` helper entrypoint rather than trusting a client-provided helper executable path.

## Long-Term Direction

- Linux, macOS, and Windows should consume the same managed permission profile
  semantics from inherited sandbox metadata.
- Linux should support restricted reads, `:minimal`, project-root subpaths,
  temp special paths, deny paths, deny globs, and protected metadata roots.
- A future slice can add a bundled bubblewrap binary; the current repository only has system `bwrap` available.

## Phase Status

- Phase 0: completed - inspected current `mcp-repl` Linux sandbox and the
  managed permission-profile metadata contract.
- Phase 1: completed - implemented bwrap-first managed filesystem enforcement in `src/sandbox.rs`.
- Phase 2: completed - broadened Linux tests from legacy-only behavior to
  managed-profile parity.
- Phase 3: completed - full required verification passed.

## Locked Decisions

- Do not treat the inherited `codexLinuxSandboxExe` helper path as trusted
  executable input. `mcp-repl` continues to launch its own helper.
- Missing or malformed inherited sandbox metadata remains fail-closed.
- Managed filesystem profiles should not be rejected on Linux merely because they cannot be projected to legacy `workspace-write`.

## Open Questions

- Whether to vendor or bundle bubblewrap later so Linux parity does not depend on system `bwrap`.
- Whether managed-network domain allowlists should be fully proxy-routed on Linux in this slice or remain a follow-up after filesystem parity.

## Next Safe Slice

- Pick one follow-up capability and keep it narrow: bundled `bwrap`, Linux
  managed-network proxy routing, protected-create monitoring, or explicit
  feature probes for older `bwrap`.

## Stop Conditions

- Stop and ask for direction if system bubblewrap limitations make the default bwrap path unusable for normal R/Python worker startup.
- Stop and update this plan if full protected-create monitoring cannot be
  implemented in a bounded patch.

## Decision Log

- 2026-06-25: Chose a clean Linux reimplementation path because the current
  helper rejects managed profiles and uses Landlock as the primary filesystem
  sandbox, while the target managed-profile model is bwrap-first on Linux.
- 2026-06-25: Deferred bundled bubblewrap because `mcp-repl` has no existing resource bundling path for Linux helper binaries.
- 2026-06-26: Kept the helper internal rather than trusting inherited helper
  paths; inherited `useLegacyLandlock` now selects the legacy path.
- 2026-06-26: Added server-owned runtime read grants for the current executable and embedded R home under restricted-read Linux profiles so `:minimal` can start the worker without widening user-data reads.
- 2026-06-26: Synthetic bubblewrap parent mount targets are non-writable, so a writable session temp child does not imply ambient `/tmp` writes.
- 2026-06-26: Linux bwrap-backed R interrupts target sandbox descendants instead of the bwrap monitor, preserving persistent REPL interrupt behavior.
- 2026-06-26: Full required verification passed for this slice: `cargo check`, `cargo build`, integration runner, clippy with warnings denied, `cargo test --quiet`, and `cargo +nightly fmt`.

## Remaining Follow-ups

- Vendor or bundle bubblewrap so Linux parity does not depend on a system `bwrap`.
- Add Linux managed-network proxy routing for domain allowlists instead of the current fail-closed behavior.
- Add protected-create monitoring for missing protected metadata paths. This
  slice masks and cleans synthetic metadata mount targets, but it does not yet
  run the full inotify-based create monitor.
- Probe bubblewrap feature support more precisely for older system `bwrap` builds.
