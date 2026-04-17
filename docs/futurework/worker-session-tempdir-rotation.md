# Future Work: Per-Worker Session Tempdir Rotation

## Summary

Potential follow-on: stop treating the worker session tempdir as one stable path
that must be reset in place before every spawn.

Instead:

- allocate a fresh server-owned temp path for each worker launch,
- pass that fresh path to the worker as its temp root,
- treat cleanup of older temp paths as best-effort follow-up work rather than a
  hard spawn prerequisite.

## Why This Matters

Today `SandboxState` chooses one session tempdir path for the server lifetime,
and each worker spawn clears and recreates that same path in place.

That is simple, but it couples new worker startup to successful deletion of the
old temp tree.

Failure modes include:

- a stray worker descendant still holding a file or directory handle,
- antivirus or indexer activity touching files under the temp tree,
- an incidental external process inspecting a file at the wrong moment,
- Windows-specific delete semantics that reject removing directories with open
  handles.

In the current design, those failures can block the next worker launch even
though the old temp tree is logically dead.

## Current Behavior

- The server allocates one tempdir path in `SandboxState`.
- Worker spawn calls `prepare_session_temp_dir()` before every launch.
- `prepare_session_temp_dir()` resets the same path in place by
  `remove_dir_all()` followed by `create_dir_all()`.
- If that delete step fails, the new worker launch fails.

This behavior is intentional today, but it is a lifecycle tradeoff rather than
the desired long-term contract.

## Intended Direction

- Introduce one stable server-owned temp root for the session lifetime.
- Allocate one fresh child tempdir under that root per worker launch.
- Point `TMPDIR`, `TEMP`, `TMP`, and `MCP_REPL_R_SESSION_TMPDIR` at the fresh
  child path for that launch only.
- Keep old temp generations isolated from later launches even when cleanup is
  delayed.
- Make cleanup of prior generations best-effort and retryable instead of a hard
  gate for the next spawn.

## Desired Outcomes

- A stale temp file or directory must not block the next worker launch.
- New workers must never inherit temp contents from earlier launches.
- Temp cleanup failures should degrade into bounded disk leakage plus logging,
  not session unavailability.
- The Windows sandbox model should keep temp ACLs launch-scoped even when the
  temp path rotates every launch.

## Relationship To Other Work

- This is related to, but separate from, stronger worker descendant
  containment.
- Even if descendant cleanup improves, tempdir rotation is still useful because
  incidental external readers can also interfere with delete-in-place cleanup.
- This is also separate from `docs/futurework/per-turn-history-bundles.md`;
  turn history should remain server-owned state, not worker temp state.

## Possible Follow-On Slice

- Split the current tempdir concept into:
  - a stable server-owned temp root,
  - a per-launch child tempdir.
- Keep startup-log persistence working when the launch tempdir becomes
  generation-specific.
- Add bounded cleanup/retry of older temp generations on worker exit and/or
  server shutdown.
- Add public regression coverage that simulates a tempdir delete failure and
  asserts that the next worker launch falls forward to a fresh temp path.

## Non-Goals

- Changing the public `repl` or `repl_reset` API.
- Reusing worker tempdirs for persistent session history.
- Solving descendant process containment by itself; that is related but
  separate work.
