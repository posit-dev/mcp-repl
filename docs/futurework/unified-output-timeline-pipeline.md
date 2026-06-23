# Unified Output Timeline Pipeline

## Summary

The server currently has more divergence between pager mode and files mode than
it should.

The current implementation now has a shared resolved output renderer, but still
uses separate buffering stores for files mode and pager mode. Longer term, the
cleaner design is:

- collect output into one shared in-memory timeline model,
- keep timeline resolution, generated input echo presentation, image ordering,
  and related formatting in the shared resolved-output layer,
- defer the pager-vs-files split until the final presentation step.

## Why This Is Deferred

The current branch is intentionally narrower:

- fix the plot-image ordering bug,
- do the minimum refactoring required to fix it correctly,
- avoid broad output-pipeline redesign while landing that fix.

That bug is now fixed in a way that improves sharing. The shared renderer lives
in `src/resolved_output.rs`, but the overall split between
`src/pending_output_tape.rs` and the pager/output-ring path remains larger than
ideal.

## Current Friction

- Files mode and pager mode still buffer different intermediate structures.
- Architectural discovery is still harder than necessary because there is no
  single backing timeline store that both modes consume.
- Files mode still owns request sealing, UTF-8 tail flushing, timeout staging,
  and transcript retention; pager mode still owns byte cursors, page ranges,
  active pager state, and search state.

## Intended Direction

- Keep using one canonical resolved output-timeline structure for server-side
  request output.
- Keep cross-channel ordering and generated echo presentation in that shared
  layer.
- Let files mode decide how to seal or spill that resolved timeline.
- Let pager mode decide how to page or elide that same resolved timeline.
- Keep mode-specific behavior focused on presentation, retention, and paging
  policy rather than duplicated merge logic.

## Possible Follow-On Slices

- Make `pending_output_tape` and the pager/output-ring path converge on that
  backing store.
- Move more reply-seal formatting out of mode-specific code and into shared
  helpers.
- Re-evaluate whether some current files-mode and pager-mode tests should become
  shared behavior tests over the common timeline layer.

## Non-Goals For The Current Branch

- Replacing both buffering systems with one backing store immediately.
- Redesigning oversized-output retention policy.
- Expanding the plot-ordering fix into a full output-subsystem rewrite.
