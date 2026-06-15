# Sideband V3 Cleanup

## Summary

- Remove worker protocol v2 compatibility and stale request-accounting docs.
- Keep the current public `repl` and `repl_reset` tool behavior unchanged.

## Status

- State: completed
- Last updated: 2026-06-15
- Current phase: completed

## Current Direction

- Treat worker protocol version 3 as the only supported worker/server
  contract.
- Keep request completion owned by worker-emitted `idle` or `session_end`
  events for protocol workers.
- Delete tests, fixture branches, and documentation that exist only for the
  retired v2 custom-worker protocol.

## Phase Status

- Phase 0: completed - identify stale v2 compatibility surfaces.
- Phase 1: completed - remove v2 custom-worker compatibility and docs.
- Phase 2: completed - run the required checks.

## Locked Decisions

- Do not retain wire compatibility for protocol version 2.
- Do not document server-side readline byte accounting as the current
  completion model.

## Open Questions

- None for this cleanup slice.

## Next Safe Slice

- No remaining slice for this plan.

## Stop Conditions

- Stop before changing the public `repl` reply shape.
- Stop before replacing the built-in Python stdin transport design.

## Decision Log

- 2026-06-15: Bound this cleanup to the superseded protocol-version and
  custom-worker compatibility paths so the branch stays mostly deletion-focused.
- 2026-06-15: Removed the v2 Zod/custom-worker conformance path, deleted stale
  active plans, updated protocol docs, and validated with the required checks.
