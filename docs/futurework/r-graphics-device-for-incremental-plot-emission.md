# R Graphics Device For Incremental Plot Emission

## Summary

The current R plot-capture path is good enough for many cases, but it cannot
reliably surface a plot as soon as it becomes visible on an interactive device.

The most obvious failure shape is a grouped top-level expression such as:

```r
local({ plot(1:10); Sys.sleep(2); cat("done\n") })
```

In an interactive R session, the plot is already visible before the later
`cat("done\n")`. In `mcp-repl`, the current hook-based capture path can delay
the plot image until later than that.

That timing gap is also visible at timeout boundaries. A short `write_stdin`
timeout can return before the worker has replayed the recorded plot into a PNG,
so under heavier load the same request may surface the image in the timeout
reply or in the next empty-input poll. The combined behavior is still correct,
but the exact per-reply split is not a stable contract today.

This future work item covers a more direct graphics-capture design that can
emit plots incrementally, without waiting for top-level task completion.

## Current Design

Today, `mcp-repl` captures R plots by:

- setting a custom default device in `r/mcp_repl.R`,
- enabling the display list,
- observing `before.plot.new` / `before.grid.newpage`,
- taking `recordPlot()` snapshots,
- replaying those snapshots into a PNG device,
- emitting the resulting PNG over IPC.

That path is intentionally lightweight and avoids implementing a full graphics
device in Rust or C.

## Why This Is Insufficient

The current hook set is too coarse for some timing-sensitive cases.

Important upstream constraints:

- `addTaskCallback()` runs only after a top-level task completes, not after an
  intermediate drawing operation inside a grouped expression.
- `before.plot.new` and `plot.new` bracket frame advances, not “the current
  page just became visible”.

So for a grouped expression, the current design has no precise “plot is now on
screen” signal. That means server-side timeline fixes can only help after the
image exists; they cannot make an image arrive earlier than the worker emits it.

## Candidate Approaches

### 1. Real MCP graphics device

Implement a real R graphics device for `mcp-repl`.

Why this is attractive:

- drawing callbacks run as graphics operations happen,
- the device can know immediately when a page becomes dirty,
- the worker can emit a plot image before later non-graphics code such as
  `Sys.sleep()` or `cat()` completes,
- this matches interactive-device timing more closely than the current replay
  path.

Tradeoffs:

- materially larger implementation,
- device correctness work for page lifecycle, clipping, text, rasters, and
  sizing,
- likely more cross-platform complexity.

### 2. Custom offscreen capture device

Implement a narrower custom device whose job is only to capture the active page
to an offscreen raster surface and emit snapshots when that surface changes.

Why this may be a good middle ground:

- still gives draw-time visibility into graphics updates,
- avoids the extra `recordPlot()` -> `replayPlot()` step,
- keeps the output format simple: emit PNG snapshots, dedupe when unchanged.

Tradeoffs:

- still requires device implementation work,
- still needs careful handling of page boundaries and device state,
- may end up close in complexity to a full custom device anyway.

### 3. Deeper graphics-engine integration without a device

Investigate whether a lower-level hook into the R graphics engine could observe
incremental plot updates while keeping the current device/replay structure.

Current read on this option:

- it is less promising than it sounds,
- the exposed engine lifecycle callbacks are oriented around state init/save/
  restore and replay validation rather than fine-grained draw notifications,
- it is unlikely to provide a clean “current page changed” event for the
  existing architecture.

This option should be treated as a long shot, not the default plan.

## Recommendation

If `mcp-repl` needs grouped expressions to surface plots before later stdout
text, the serious path is a custom graphics device.

If the goal is the smallest architectural leap from today’s code:

- prefer a custom offscreen capture device first,
- keep the emitted artifact as PNG,
- dedupe on content hash as today,
- leave server-side ordering logic unchanged except for consuming earlier image
  arrivals.

If the goal is the cleanest long-term architecture:

- implement a real `mcp-repl` graphics device explicitly,
- stop depending on `recordPlot()` / `replayPlot()` for timing-sensitive plot
  emission.

## Relationship To Other Work

This is separate from:

- `docs/futurework/r-worker-simplification-and-server-inferred-completion.md`
- `docs/futurework/r-embedding-minimal-callbacks.md`

Those notes are about worker semantics and embedding complexity. This note is
specifically about the plot-capture mechanism.

## Non-Goals

- Fixing plot/stdout ordering only in the server timeline.
- Expanding the existing `recordPlot()` hook set indefinitely.
- Redesigning Python plotting.
- Redefining request completion as part of the same work item.
