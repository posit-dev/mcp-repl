# Built-In R Worker Turn Boundary Simplification

## Summary

This future work item covers the larger simplification goal for the built-in R
adapter:

- keep the R worker thin and factual,
- let it run the ordinary embedded REPL,
- move request input behind the v3 `turn_start` boundary,
- complete successful turns with worker-emitted `idle` or `session_end`.

This is intentionally broader than the current branch milestone.

## Status

Server-inferred completion is no longer the intended direction. The v3 worker
protocol makes completion worker-owned: a turn completes only when the worker can
truthfully emit `idle` for that turn, or when the worker emits `session_end`.
The built-in R adapter still uses private raw-stdin bridge messages and
server-side adapter bookkeeping. The remaining work is to move that built-in
adapter behind the same `turn_start` and `idle` shape used by custom protocol
workers.

The original motivation was broader than one bug fix:

- keep the worker thin and factual,
- keep timeline interpretation in the server,
- avoid expanding plot/image ordering code with server-owned prompt-completion
  heuristics.

## Intended Direction

- For R specifically, the worker should own the request boundary and emit
  `idle(turn_id)` only after it can prove the runtime is waiting for new input
  and no input from the active turn remains queued or buffered.
- Timeline processing should stay in the server and remain independent from
  completion. Mixed stdout/image ordering bugs should be fixed in the server's
  merge layer, not by reintroducing prompt-shaped completion guesses.
- Private adapter events such as readline byte accounting should become
  unnecessary once R request input moves behind `turn_start`.

## Likely Follow-On Work

- Replace the built-in R raw-stdin request bridge with worker-owned
  `turn_start` input handling.
- Emit `input_line` for logical R input lines and `idle` for successful
  same-worker completion.
- Decide whether any remaining plot-capture state belongs in the worker or in
  server-side timeline assembly once R uses the v3 turn boundary.
- Re-evaluate the current R plot-capture mechanism independently from
  request-completion semantics.

## Non-Goals For The Current Branch

- Redesigning Python completion.
- Landing the full built-in R `turn_start` migration.
