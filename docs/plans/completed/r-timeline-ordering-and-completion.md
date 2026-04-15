# R Timeline Ordering And Completion

## Summary

- Fix R plot/image ordering in server-side timeline processing without adding new IPC fields.
- Do the refactoring necessary to make that fix correct and maintainable, but no more.
- Do not broaden this branch into worker/protocol simplification or Python redesign.

## Status

- State: completed
- Last updated: 2026-04-15
- Current phase: completed

## Current Direction

- Phase 1 fixed the visible R plot/image ordering bug in server-side timeline processing.
- The server now uses existing sideband facts to anchor image events before later echoed input and its later stdout, without adding new IPC protocol fields.
- The broader R-worker/server-completion simplification remains separate future work.

## Phase Status

- Phase 0: completed
- Phase 1: completed
- Phase 2: deferred

## Locked Decisions

- Do not add new IPC protocol fields for the ordering fix.
- Do not broaden this slice into worker-completion redesign or a Python completion redesign.
- Keep the fix centered in server-side timeline/tape processing.

## Delivered Slice

- Removed the experimental `stdout_bytes_before` plumbing.
- Restored the wire protocol and docs to the pre-field shape.
- Added a shared server-side output timeline helper and used it in both pager mode and files mode reply construction.
- Added focused regression coverage for the plot-before-later-stdout bug.

## Stop Conditions

- Stop if the server-side ordering fix starts depending on speculative worker-state inference that is not encoded by existing R events.
- Stop if the change would alter Python completion behavior in this slice.

## Decision Log

- 2026-04-15: Decided to land the work over multiple PRs. This branch is limited to the reported ordering bug plus the refactoring required to fix it correctly.
- 2026-04-15: Moved the broader “simplify the R worker and infer completion on the server” idea out of the active plan and into future work so it does not expand this milestone.
- 2026-04-15: Completed the branch milestone by fixing image/stdout ordering in server-side timeline reconstruction without changing the IPC wire contract.
