# Inherit Sandbox Restart Contract

## Summary

- Change `--sandbox inherit` so new per-tool-call sandbox metadata takes effect by restarting the worker at the next non-poll, non-bare-interrupt interaction.
- Keep empty-input polls and bare `Ctrl-C` as the two cases that do not force a restart on sandbox change.
- Keep explicit non-`inherit` sandbox modes authoritative and unchanged.

## Status

- State: completed
- Last updated: 2026-04-19
- Current phase: completed

## Current Direction

- Treat sandbox changes as worker-session boundaries.
- If current tool-call metadata changes the effective inherited sandbox, restart the worker before handling any tool call that would otherwise send input, restart the worker, or otherwise interact with the worker statefully.
- Empty-input polls keep draining existing state without forcing a restart.
- A bare `Ctrl-C` remains a local recovery control and does not force a restart just because sandbox metadata changed.

## Long-Term Direction

- The inherit contract should stay simple enough to explain in one paragraph:
  fresh worker interaction uses the current metadata, and sandbox changes reset the session before that interaction happens.
- Review-driven exceptions should be minimized; only the explicit poll and bare interrupt escape hatches should remain.

## Phase Status

- Phase 0: completed
  - Identified that the current fail-closed follow-up split is not the desired product behavior.
- Phase 1: completed
  - Reworked runtime sequencing around restart-on-change semantics.
- Phase 2: completed
  - Refreshed tests, docs, and final verification.

## Locked Decisions

- Do not revert to the obsolete async sandbox update protocol.
- Do not let explicit non-`inherit` CLI sandbox modes depend on Codex metadata.
- If the inherited sandbox changes, the next non-poll, non-bare-interrupt interaction should reset the worker instead of trying to preserve the old request/session.

## Open Questions

- What exact restart notice text best communicates both the restart cause and the new effective sandbox policy without creating brittle snapshots?
- Should a bare `Ctrl-C` with no live worker remain a no-op control reply or continue to surface the existing idle/session behavior?

## Decision Log

- 2026-04-19: Replaced the earlier fail-closed control-tail contract with a restart-on-change contract for non-poll, non-bare-interrupt interactions.
- 2026-04-19: Landed the runtime, docs, and regression updates; restart notices are informational and initial inherit-mode spawns do not pretend they were restarts.
