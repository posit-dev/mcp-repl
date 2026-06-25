# Managed Sandbox Metadata

## Summary

- Replace the macOS worker sandbox model with managed per-call metadata: explicit filesystem entries, network policy, deny-read roots, deny globs, and protected metadata carveouts.
- Keep the current `mcp-repl` CLI arguments available. Older metadata formats are out of scope.

## Status

- State: completed
- Last updated: 2026-06-23
- Current phase: complete

## Current Direction

- Preserve the CLI-facing legacy sandbox modes as configuration sugar.
- Convert those modes into the same runtime policy evaluator used for current `permissionProfile` metadata.
- Render managed filesystem and network entries directly into the macOS Seatbelt policy.

## Long-Term Direction

- The worker launch path should treat filesystem and network runtime permissions as first-class state.
- Linux and Windows can keep their current compatibility projection for now; this task is scoped to macOS behavior.

## Phase Status

- Phase 0: completed - inspected current `mcp-repl` sandbox behavior and live inherited metadata shape.
- Phase 1: completed - replace metadata parsing and macOS seatbelt generation.
- Phase 2: completed - update docs and run required checks.

## Locked Decisions

- Current per-call metadata is authoritative for `--sandbox inherit`.
- Do not preserve compatibility with older metadata formats.
- Fail fast on malformed paths and unsupported runtime policy entries.

## Open Questions

- How closely Claude Code will mirror this per-call metadata is still unknown.

## Next Safe Slice

- Track Claude Code metadata differences once its sandbox contract is concrete.

## Stop Conditions

- Stop and ask if supporting another client requires broadening `mcp-repl` beyond the existing CLI arguments or introducing a network dependency.

## Decision Log

- 2026-06-23: Chose a local runtime policy layer so the binary remains self-contained and the change stays focused on worker sandbox behavior.
- 2026-06-23: Preserved the existing CLI modes as compatibility sugar while rendering managed filesystem profiles directly into macOS Seatbelt.
- 2026-06-23: Added the server-owned session temp directory as an explicit writable root for macOS worker launches so read-only and restricted profiles can still start R and Python.
- 2026-06-23: Kept `:minimal` platform defaults as a separate Seatbelt include so broader system reads apply only when the profile requests them; added mcp-repl R/Python runtime roots, the libomp shared-memory allowance, and debug embedding for harp's R modules after focused minimal-profile regressions.
