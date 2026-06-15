# R Graphics Device For Incremental Plot Emission

## Summary

A custom R graphics device was investigated as a way to emit plot updates closer
to draw time. It is not currently recommended.

The known plot-before-later-stdout ordering bug was a server-side timeline
problem, not a graphics-device problem. That bug is now handled by timeline
reconstruction, and a custom device would not remove the need for that ordering
logic.

The current R plot-capture path still has one real limitation: it cannot reliably
surface a plot as soon as it becomes visible on an interactive device inside a
single grouped top-level expression.

The most obvious failure shape is a grouped top-level expression such as:

```r
local({ plot(1:10); Sys.sleep(2); cat("done\n") })
```

In an interactive R session, the plot is already visible before the later
`cat("done\n")`. In `mcp-repl`, the current hook-based capture path can delay
the plot image until later than that.

This note is retained as a decision record for why that limitation is not enough
to justify a custom graphics device today.

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

## Remaining Limitation

The current hook set is too coarse for some timing-sensitive cases.

Important upstream constraints:

- `addTaskCallback()` runs only after a top-level task completes, not after an
  intermediate drawing operation inside a grouped expression.
- `before.plot.new` and `plot.new` bracket frame advances, not “the current
  page just became visible”.

So for a grouped expression, the current design has no precise “plot is now on
screen” signal. That means server-side timeline fixes can only help after the
image exists; they cannot make an image arrive earlier than the worker emits it.

That limitation is separate from ordinary plot/stdout ordering across separate
input lines. The server timeline can already place an emitted `plot_image`
before later stdout when the sideband facts contain that ordering.

## Investigation Outcome

Do not implement a custom graphics device now.

The reasons are:

- The main observed ordering failure was fixed in server-side timeline
  reconstruction.
- A custom device would not by itself solve cross-stream ordering; the server
  would still need to merge stdout/stderr pipes with sideband image events.
- The remaining grouped-expression limitation is narrower than the original
  ordering problem.
- A device implementation would add a large native graphics surface: page
  lifecycle, clipping, text, rasters, sizing, platform behavior, and replay
  compatibility.
- Lower-level graphics-engine hooks without a device do not appear to expose a
  clean "current page changed" signal for the existing architecture.

Keep the hook/replay path unless there is a concrete product requirement for
mid-expression plot visibility.

## Investigated Approaches

### 1. Real MCP graphics device

Implement a real R graphics device for `mcp-repl`.

Why this looked attractive:

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

Current decision: not recommended.

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

Current decision: not recommended. The narrower version still carries most of
the hard device-lifecycle work.

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

## Revisit Criteria

Revisit this only if there is a concrete public requirement for plots to appear
while a single grouped R expression is still running.

Before changing the graphics device, require:

- a public API regression test that demonstrates the user-visible gap,
- a prototype showing materially earlier image emission for that case,
- evidence that the improvement is worth the native-device complexity,
- no regression to current base and grid plot capture.

## Relationship To Other Work

This is separate from:

- `docs/futurework/r-worker-simplification-and-server-inferred-completion.md`
- `docs/futurework/r-embedding-minimal-callbacks.md`

Those notes are about worker semantics and embedding complexity. This note is
specifically about the plot-capture mechanism.

## Non-Goals

- Reopening the fixed plot/stdout ordering bug.
- Expanding the existing `recordPlot()` hook set indefinitely.
- Redesigning Python plotting.
- Redefining request completion as part of the same work item.
