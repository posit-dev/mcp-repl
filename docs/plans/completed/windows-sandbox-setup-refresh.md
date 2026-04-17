# Windows Sandbox Setup Refresh

## Summary

- Moved Windows sandbox ACL preparation out of the wrapper launch hot path and into a parent-side prepared-launch model.
- Kept capability identity deterministic and non-persistent.
- Split stable workspace ACL preparation from launch-scoped overlay and session-temp ACL handling.

## Status

- State: completed
- Last updated: 2026-04-16
- Current phase: completed

## Delivered Slice

- Added a parent-owned prepared-launch cache keyed by effective sandbox policy, cwd, and session temp dir.
- Made the internal Windows wrapper require prepared launch state instead of computing ACLs inline on fallback paths.
- Refreshed prepared workspace ACLs before spawn while keeping session temp access launch-scoped.
- Kept the Windows fault-injection harness in its own `src/windows_sandbox_test_support.rs` module.

## Locked Decisions

- Do not add persistent Windows sandbox metadata files.
- Deterministic capability SIDs remain the identity mechanism for prepared workspace ACL state.
- Session temp trees stay launch-scoped even when same-checkout sessions share a prepared filesystem SID.
- The broader stdin ownership redesign remains separate future work.

## Follow-On Work

- Broader stdin ownership and transport cleanup remains future work in `docs/futurework/stdin-transport-single-owner.md`.
- Per-launch tempdir rotation remains future work in `docs/futurework/worker-session-tempdir-rotation.md`.
- Stronger worker descendant containment remains future work in `docs/futurework/stronger-worker-child-containment.md`.
- If Windows eventually needs explicit steady-state prepared-ACL cleanup or revocation, capture that as a separate futurework item instead of reopening this rollout plan.

## Decision Log

- 2026-04-07: Chose an in-memory parent-side setup cache rather than persistent sandbox metadata.
- 2026-04-11: Required prepared launch state and kept session temp children launch-scoped.
- 2026-04-16: Closed the plan after PR #33 landed and the wrapper fallback and ACL helper split were both in place.
