# Unified Output Timeline Pipeline

## Summary

The server currently has more divergence between pager mode and files mode than
it should.

Longer term, the cleaner design is:

- collect output into one shared in-memory timeline model,
- perform timeline resolution, echo suppression, image ordering, and related
  formatting through one mostly common code path,
- defer the pager-vs-files split until the final presentation step.

## Why This Is Deferred

The current branch is intentionally narrower:

- fix the plot-image ordering bug,
- do the minimum refactoring required to fix it correctly,
- avoid broad output-pipeline redesign while landing that fix.

That bug is now fixed in a way that improves sharing, but the overall split
between `src/pending_output_tape.rs` and the pager/output-ring path remains
larger than ideal.

## Current Friction

- Files mode and pager mode still buffer different intermediate structures.
- Echo suppression and timeline resolution now share more logic, but the
  surrounding assembly path still diverges.
- Architectural discovery is harder because there is no single “resolved
  timeline” abstraction that both modes consume.

## Intended Direction

- Introduce one canonical resolved output-timeline structure for server-side
  request output.
- Keep cross-channel ordering and echo handling in that shared layer.
- Let files mode decide how to seal or spill that resolved timeline.
- Let pager mode decide how to page or elide that same resolved timeline.
- Keep mode-specific behavior focused on presentation, retention, and paging
  policy rather than duplicated merge logic.

## Possible Follow-On Slices

- Define the canonical resolved timeline type and its invariants.
- Make `pending_output_tape` and the pager/output-ring path converge on that
  type.
- Move more reply-seal formatting out of mode-specific code and into shared
  helpers.
- Re-evaluate whether some current files-mode and pager-mode tests should become
  shared behavior tests over the common timeline layer.

## Non-Goals For The Current Branch

- Replacing both buffering systems with one abstraction immediately.
- Redesigning oversized-output retention policy.
- Expanding the plot-ordering fix into a full output-subsystem rewrite.
