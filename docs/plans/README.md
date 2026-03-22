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

## Template

Create a Markdown file in `docs/plans/active/` with these headings:

```md
# <Title>

## Summary

- What changes.
- What stays unchanged.

## Status

- State: active
- Owner: <agent or person>
- Last updated: YYYY-MM-DD

## Decision Log

- YYYY-MM-DD: key decision and why it was made.
```

Add extra sections only when they reduce ambiguity.

## Lifecycle

1. Start the plan in `docs/plans/active/`.
2. Update the `## Status` and `## Decision Log` as decisions change.
3. Move the plan to `docs/plans/completed/` when the work lands or is intentionally abandoned.
4. Capture recurring follow-up items in `docs/plans/tech-debt.md` instead of leaving them buried in old plans.
