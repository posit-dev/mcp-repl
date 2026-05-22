# R Worker Simplification And Server-Inferred Completion

## Summary

This future work item covers the larger simplification goal behind the current ordering investigation:

- keep the R worker thin and factual,
- let it run the ordinary embedded REPL,
- emit runtime facts such as `readline_start`, byte-level input accounting, and image events over IPC,
- move request-boundary interpretation and timeline reconstruction into the server.

This is intentionally broader than the current branch milestone.

## Status

This simplification has started: request completion is now inferred by the
server from prompt/readline sideband facts instead of a worker-emitted
request-boundary message. The remaining work is to simplify worker code around
that contract and broaden the design where needed.

The original motivation was broader than one bug fix:

- keep the worker thin and factual,
- keep timeline interpretation in the server,
- avoid expanding plot/image ordering code with worker-owned request-boundary
  state.

## Intended Direction

- The server should infer more of the logical request timeline from factual worker events instead of relying on worker-owned request-boundary decisions.
- For R specifically, the worker should not contain complex request-end semantics beyond exposing what `readline` and plotting actually did.
- Timeline processing should become strong enough that mixed stdout/image ordering bugs are fixed in the server’s merge layer rather than by extending the IPC payload with speculative worker-side state.

## Likely Follow-On Work

- Separate top-level idle-prompt detection from nested prompt flows such as `browser()` and `readline()`.
- Decide whether the same simplification should apply to Python or whether Python should keep a different completion contract.
- Re-evaluate the current R plot-capture mechanism independently from request-completion semantics.

## Non-Goals For The Current Branch

- Redesigning Python completion.
- Landing a broader R worker architecture rewrite.
