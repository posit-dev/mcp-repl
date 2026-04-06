# Execution Plans

Use checked-in execution plans for work that is too large or too cross-cutting to keep only in prompt context.

## When to Write a Plan

Write a plan when the change:

- spans multiple files or subsystems,
- changes public behavior or protocol contracts,
- changes both R and Python behavior,
- is expected to take more than one PR, or
- needs explicit decision logging so a later agent can pick it up safely.

Small bugfixes, typo fixes, and isolated docs-only changes do not need a checked-in plan.

For multi-phase refactors or redesigns, prefer one living plan per initiative. Keep updating that plan as
decisions change instead of creating a new file for each small pass.

## Template

Create a Markdown file in `docs/plans/active/` with these headings:

```md
# <Title>

## Summary

- What changes.
- What stays unchanged.

## Status

- State: active
- Last updated: YYYY-MM-DD
- Current phase: <planning|implementation|validation|paused>

## Current Direction

- The design currently being pursued.
- Why it is the preferred path right now.

## Long-Term Direction

- The intended end-state architecture, if it differs from the current bounded phase.
- What parts of the current phase are temporary tactics rather than the long-term design.

## Phase Status

- Phase 0: completed / active / pending
- Phase 1: completed / active / pending

## Locked Decisions

- Decision that should not be re-litigated without new evidence.

## Open Questions

- Question that still needs a decision.

## Next Safe Slice

- The next bounded piece of work that is safe to implement now.

## Stop Conditions

- Condition that should cause the agent to stop, update the plan, and ask for a decision.

## Decision Log

- YYYY-MM-DD: key decision and why it was made.
```

Add or remove sections only when it reduces ambiguity. The goal is to preserve design history and current state
without turning the plan into a changelog.

When the current phase deliberately takes a simpler path for iteration, say so explicitly and record the tradeoff.
Do not let a temporary implementation tactic become the apparent product definition just because it is the current slice.

## Lifecycle

1. Start the plan in `docs/plans/active/`.
2. Update `## Status`, `## Open Questions`, `## Next Safe Slice`, and `## Decision Log` as the work evolves.
3. Before pausing or handing off a non-trivial initiative, update the plan so the next agent does not need to
   rediscover the current state.
4. Move the plan to `docs/plans/completed/` when the work lands or is intentionally abandoned.
5. Capture recurring follow-up items in `docs/plans/tech-debt.md` instead of leaving them buried in old plans.
