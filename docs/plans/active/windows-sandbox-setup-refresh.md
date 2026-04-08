# Windows Sandbox Setup Refresh

## Summary

- Move Windows ACL preparation out of the wrapper launch hot path and into a parent-side setup step.
- Keep capability identity deterministic and non-persistent; do not add new on-disk metadata.
- Preserve the existing wrapper as the process/token launcher, but let it skip filesystem ACL work when setup already ran.

## Status

- State: active
- Last updated: 2026-04-07
- Current phase: implementation

## Current Direction

- Add a parent-owned Windows setup cache keyed by sandbox policy, cwd, and session temp dir.
- Prepare filesystem ACL state once per effective sandbox configuration and pass the prepared capability SID into the wrapper.
- Keep the wrapper able to fall back to inline preparation when invoked directly without prepared state.

## Long-Term Direction

- The end state should resemble the `codex` split between setup/refresh and launch, without introducing a persistent SID registry or other checked-in/local metadata files.
- This phase is intentionally narrower than the full `codex` architecture: it keeps the existing token-launch wrapper and focuses first on removing launch-path ACL churn and double work.

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
- Whether direct `--windows-sandbox` invocation should remain a fully supported fallback path or become debug-only behavior.

## Next Safe Slice

- Introduce a parent-side Windows setup helper and cache.
- Thread prepared capability SID information from `WorkerManager` into the Windows wrapper invocation.
- Skip ACL mutation in the wrapper when prepared state is supplied.

## Stop Conditions

- Stop if the parent-side cache needs to become persistent to remain correct.
- Stop if wrapper fallback semantics become ambiguous enough that direct invocation would diverge from normal server behavior in unsafe ways.

## Decision Log

- 2026-04-07: Start with an in-memory setup cache rather than a persistent registry because the current requirement is to avoid polluting disk with sandbox metadata while still removing launch-path ACL work.
- 2026-04-07: Reset the per-session temp directory before Windows ACL preparation, and avoid re-resetting it during command preparation. Recreating the temp dir after ACL setup drops the prepared permissions and causes Windows worker startup failures like `Fatal error: cannot create 'R_TempDir'`.
