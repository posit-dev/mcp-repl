# R Embedding With Minimal Callbacks

## Summary

`mcp-repl` likely does more custom embedded-R setup than necessary.

The intended simplification is:

- use R's stock embedded startup path as much as possible on each platform,
- let R keep its built-in console write/message/busy/suicide behavior where feasible,
- override only the readline path needed to integrate with server-owned input.

In short: the embedding layer should do less.

## Why This Is Deferred

The current branch fixed the plot-image ordering bug in server-side timeline
processing. It intentionally did not redesign R initialization.

This follow-on is architectural cleanup:

- simplify R embedding,
- reduce worker-owned callback code,
- make it clearer that output semantics belong in server capture/timeline code.

That is valuable, but it is a separate change from the ordering fix.

## Motivation

Upstream embedded R already provides more startup behavior than `mcp-repl`
currently relies on.

On Unix in particular, stock embedded startup already installs standard console
callbacks during `setup_Rmainloop()`, so there is a plausible path to a much
thinner integration layer.

More generally, the product goal is the same on both platforms:

- console output routing through R's normal embedded callback path,
- message handling,
- busy/suicide behavior.

If `mcp-repl` only truly needs to control input, then overriding write and other
callbacks adds complexity without a strong architectural benefit.

## Intended Direction

- Keep the custom `ReadConsole` integration.
- Stop supplying custom write/message/busy/suicide callbacks unless a specific
  product behavior requires them.
- Prefer stock embedded-R behavior for console output forwarding where the
  platform allows it.
- Treat any remaining worker-local callback glue as backend integration detail,
  not as part of the output architecture.

## Important Tradeoff

The stock embedded console path does not necessarily preserve the same
stdout/stderr distinction that the current custom callback path preserves on all
platforms.

So this simplification should be evaluated explicitly as a product tradeoff:

- if only readline ownership matters, stock callbacks may be preferable,
- if stream attribution is still required, some callback customization may need
  to remain.

## Platform Shape

- Unix likely allows more aggressive simplification sooner.
- Windows has tighter embedding constraints and may require a different first
  slice.
- But Windows is not a separate product question: the simplification goal still
  applies there, even if the implementation cannot be identical.

## Relationship To Advisory Write Metadata

This note is about making the embedding layer do less.

If some worker-owned write callbacks remain, a separate question is whether they
should emit advisory IPC metadata about worker-owned writes for ordering or
diagnostic purposes. That direction is tracked separately in
`docs/futurework/advisory-worker-write-observations.md`.

The two questions should stay separate:

- reducing callback ownership,
- enriching metadata from the callbacks that remain.

## Possible Follow-On Slices

- Prototype a Unix startup path that overrides only `ReadConsole`.
- Compare visible stdout/stderr behavior against the current custom-callback
  path.
- Identify the smallest Windows simplification that reduces custom callback
  ownership without regressing current behavior.
- Remove worker-local output forwarding helpers if they become redundant.
- Revisit whether any remaining callback overrides are actually required.

## Non-Goals

- Changing server-side timeline ordering behavior.
- Mixing this cleanup into unrelated R worker refactors.
