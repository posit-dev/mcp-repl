# Windows Sandbox Setup Refresh

## Summary

- Move Windows ACL preparation out of the wrapper launch hot path and into a parent-side setup step.
- Keep capability identity deterministic and non-persistent; do not add new on-disk metadata.
- Preserve the existing wrapper as the process/token launcher, but let it skip filesystem ACL work when setup already ran.

## Status

- State: active
- Last updated: 2026-04-11
- Current phase: implementation

## Current Direction

- Add a parent-owned Windows setup cache keyed by sandbox policy, cwd, and session temp dir.
- Prepare filesystem ACL state once per effective sandbox configuration and pass the prepared capability SID into the wrapper.
- Keep the current Windows launcher entrypoint for this branch, but remove the legacy inline ACL fallback. Parent-prepared launch state is the only supported sandboxed worker path.

## Long-Term Direction

- The end state should resemble the `codex` split between setup/refresh and launch, without introducing a persistent SID registry or other checked-in/local metadata files.
- A future follow-up may still expose a friendlier CLI launcher, but this branch stays scoped to the Windows-specific setup/refresh split.

## Phase Status

- Phase 0: completed
- Phase 1: active
- Phase 2: pending

## Locked Decisions

- Do not add persistent Windows sandbox metadata files.
- Do not preserve backwards compatibility for the internal Windows wrapper CLI beyond what is useful for a safe rollout.
- Deterministic capability SIDs remain the identity mechanism for now.

## Open Questions

- Whether the parent-side cache should eventually own cleanup/revocation, or whether long-lived stable ACEs are acceptable for this model.

## Next Safe Slice

- Remove the launcher fallback path that computes ACLs inline when no prepared state is supplied.
- Re-split stable prepared ACL application from launch-scoped overlay application so one helper is not serving all lifecycles.

## Stop Conditions

- Stop if the parent-side cache needs to become persistent to remain correct.
- Stop if wrapper fallback semantics become ambiguous enough that direct invocation would diverge from normal server behavior in unsafe ways.

## Decision Log

- 2026-04-07: Start with an in-memory setup cache rather than a persistent registry because the current requirement is to avoid polluting disk with sandbox metadata while still removing launch-path ACL work.
- 2026-04-07: Reset the per-session temp directory before Windows ACL preparation, and avoid re-resetting it during command preparation. Recreating the temp dir after ACL setup drops the prepared permissions and causes Windows worker startup failures like `Fatal error: cannot create 'R_TempDir'`.
- 2026-04-08: Temp-dir resets must invalidate the cached prepared Windows launch state. Recreating the session temp dir at the same path destroys its prepared ACEs even though the cached launch key still matches by path.
- 2026-04-08: Deterministic capability SIDs must include the full workspace-write policy shape, not just mode plus cwd. Otherwise tightening writable roots or related flags can silently keep the old write access alive through stale ACEs.
- 2026-04-08: Parent-side Windows sandbox preparation must roll back any newly added ACEs if a later allow/deny update fails. Deterministic capability SIDs make partial prep state persistent unless errors clean up after themselves.
- 2026-04-08: The Windows wrapper must not block exit on stdout/stderr forwarding thread joins after the worker process exits. Descendants can inherit those pipe writers, so EOF on the wrapper-owned readers is not a safe shutdown condition.
- 2026-04-08: Wrapper shutdown should still give stdout/stderr forwarders a short bounded grace period to flush buffered pipe data after the child exits. Immediate thread drop avoids hangs but can truncate the tail of fast-exiting commands.
- 2026-04-08: The bounded drain should be progress-aware rather than a fixed grace window. Keep waiting while bytes are still moving, but abandon when the pipe stops making progress long enough to indicate a hung inherited writer.
- 2026-04-08: Windows embedded R must use `UImode_RTerm`. With `UImode_RGui`, even simple child-process launches like `system2("cmd", c("/c", "echo", "CMD_OK"))` can hang inside the worker.
- 2026-04-08: Launch-scoped resources such as `\\.\NUL` must use a per-launch SID even when filesystem ACLs are prepared on a stable capability SID. Otherwise concurrent sandboxes for the same workspace can revoke each other's device access on exit.
- 2026-04-08: Worker respawns should refresh the recreated session temp directory against the cached prepared launch instead of invalidating the whole Windows setup cache. Resetting the temp dir destroys only that directory's ACEs, not the prepared workspace ACLs.
- 2026-04-08: Prepared-launch cache hits must recompute and reapply the full allow/deny ACL plan, not just the session temp dir ACE. Recreated writable roots and newly created protected directories can change the effective ACL set without changing the cache key.
- 2026-04-08: Stable filesystem capability SIDs must not include the per-session temp dir path. Session temp ACLs are refreshed separately, and tying workspace identity to a fresh temp dir causes stale workspace ACE buildup across normal server restarts.
- 2026-04-09: Prepared workspace ACL state must exclude the per-session temp directory. The temp dir needs launch-scoped access so concurrent same-workspace sandboxes cannot read each other's `MCP_REPL_R_SESSION_TMPDIR` contents through the shared stable filesystem SID.
- 2026-04-10: Prepared-launch refresh must repair existing non-denied descendants under writable roots, not just the root directories. Files moved from the session temp dir into the workspace keep their launch-scoped DACL across a same-volume rename and otherwise become inaccessible after the next worker respawn.
- 2026-04-10: A same-SID allow ACE on a writable directory is only complete if it still inherits to children. Refresh must upgrade non-inheriting directory ACEs instead of treating them as cache hits.
- 2026-04-10: Keep only a Windows-only worker stdin gate in this branch. Embedded Python initialization can hang if another thread stays blocked on fd 0 during an active request, but the mitigation remains scoped to Windows behavior instead of a broader transport redesign.
- 2026-04-11: The broader stdin ownership redesign remains tracked separately in `docs/futurework/stdin-transport-single-owner.md`. Keep that futurework item even when this branch intentionally avoids broader transport changes.
- 2026-04-10: This branch stays Windows-only. Any friendlier cross-cutting launcher surface such as a `sandbox-exec` subcommand should be handled as a separate futurework item rather than folded into the Windows ACL refactor.
- 2026-04-10: Keep the Windows fault-injection harness in a dedicated `src/windows_sandbox_test_support.rs` module so runtime ACL code and test-only state do not keep expanding the same file.
- 2026-04-11: Prepared launches must preserve a child-inheritance path for direct file creates under writable roots. Installing the root-local and inherit-only ACEs in one ACL update avoids Windows collapsing the root's direct allow ACE on newly materialized roots.
- 2026-04-11: Session temp children must stay launch-scoped even when same-checkout sandboxes share a prepared filesystem SID. The temp root should get only the launch SID at runtime, and prepared launches should not put the shared filesystem SID into the token default DACL, otherwise temp children can be reopened and rewritten across live sessions by path.
