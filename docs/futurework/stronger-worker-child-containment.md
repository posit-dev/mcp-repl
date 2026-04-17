# Future Work: Stronger Worker Child Containment

## Summary

Potential follow-on: make worker child-process containment and teardown
structurally stronger, especially on Windows.

The goal is not only "try to kill more children," but "make it hard for stale
descendants to survive long enough to interfere with later sessions."

## Why This Matters

Today worker teardown is asymmetric:

- Unix has best-effort descendant signaling.
- Windows mainly manages the root worker / wrapper process and does not provide
  equally strong descendant cleanup.

That leaves room for detached descendants to survive past session end and keep
resources alive, including:

- stdio pipes,
- tempdir files,
- inherited handles,
- other launch-scoped resources.

This is one of the reasons tempdir delete-in-place can fail on Windows.

## Current Behavior

- Unix teardown signals the worker and discovered descendants.
- Windows soft termination deliberately lets the wrapper exit naturally so it
  can unwind temporary ACL state.
- Windows hard termination kills the child process but does not provide an
  equivalent descendant sweep.
- Teardown is intentionally bounded even if a detached descendant still holds
  stdio open.

That behavior is pragmatic, but it is not the strongest containment model.

## Intended Direction

- Strengthen session-scoped process containment so descendants are terminated or
  isolated when the session ends.
- Prevent stale descendants from targeting future sessions through inherited
  resources.
- Prefer OS-level containment where available instead of relying only on
  best-effort post hoc discovery.

## Desired Outcomes

- A dead worker session should not leave descendants that can keep temp files
  busy or continue writing into later sessions.
- Teardown should stay bounded without relying on loose descendants eventually
  disappearing on their own.
- Windows and Unix should move closer in isolation guarantees, even if the
  implementation differs by OS.

## Possible Directions

- Use stronger Windows process containment such as Job Objects with
  kill-on-close or equivalent child-tracking enforcement.
- Tighten Unix descendant tracking where current process-group behavior is still
  too weak.
- Pair stronger child containment with per-worker tempdir rotation so stale
  descendants cannot block new launches even when containment is imperfect.

## Relationship To Other Work

- This is separate from `docs/futurework/stdin-transport-single-owner.md`,
  which is about stdin ownership rather than process lifetime.
- This is related to `docs/futurework/worker-session-tempdir-rotation.md`,
  because stronger containment reduces, but does not eliminate, tempdir cleanup
  interference.

## Possible Follow-On Slice

- Add one Windows-specific containment layer that guarantees child cleanup on
  session end.
- Add regression coverage that proves a worker-spawned child cannot survive long
  enough to block tempdir cleanup for the next launch.
- Revisit teardown comments and cleanup paths once the stronger containment
  model is in place.

## Non-Goals

- Redefining the public REPL session model.
- Solving output-history storage or bundle retention.
- Replacing tempdir rotation; stronger containment and tempdir rotation are
  complementary.
