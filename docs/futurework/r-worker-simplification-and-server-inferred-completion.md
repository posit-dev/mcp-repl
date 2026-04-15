# R Worker Simplification And Server-Inferred Completion

## Summary

This future work item covers the larger simplification goal behind the current ordering investigation:

- keep the R worker thin and factual,
- let it run the ordinary embedded REPL,
- emit runtime facts such as `readline_start`, `readline_result`, and plot/image events over IPC,
- move request-boundary interpretation and timeline reconstruction into the server.

This is intentionally broader than the current branch milestone.

## Why This Is Deferred

The immediate milestone is narrower:

- fix the reported bug where an R plot image can appear after later stdout text,
- do the refactoring required to fix that ordering correctly,
- avoid expanding the branch into a full completion-model redesign.

The current server and both backends still depend on `request_end` in multiple places. Removing or redefining that contract safely needs a dedicated phase.

## Intended Direction

- The server should infer more of the logical request timeline from factual worker events instead of relying on worker-owned request-boundary decisions.
- For R specifically, the worker should not contain complex request-end semantics beyond exposing what `readline` and plotting actually did.
- Timeline processing should become strong enough that mixed stdout/image ordering bugs are fixed in the server’s merge layer rather than by extending the IPC payload with speculative worker-side state.

## Likely Follow-On Work

- Revisit whether `request_end` is needed for R once server-side completion inference is well-defined.
- Separate top-level idle-prompt detection from nested prompt flows such as `browser()` and `readline()`.
- Decide whether the same simplification should apply to Python or whether Python should keep a different completion contract.
- Re-evaluate the current R plot-capture mechanism independently from request-completion semantics.

## Non-Goals For The Current Branch

- Removing `request_end` across the whole system.
- Redesigning Python completion.
- Landing a broader R worker architecture rewrite.
