The r repl tool executes R code in a persistent session. Returns stdout, stderr, and rendered plots.

Arguments:
- `input` (string): R code to execute. Send empty string to poll for output from long-running work.
- `timeout_ms` (number, optional): Max milliseconds to wait (bounds this call only; doesn't cancel backend work).

Behavior:
- Session state (variables, loaded packages) persists across calls. Errors don't crash the session.
- Uses the user's R installation and library paths.
- Plots (ggplot2 and base R) are captured and returned as images. Adjust sizing with `options(console.plot.width, console.plot.height, console.plot.units, console.plot.dpi)`.
- Large output uses files-first overflow by default: the reply keeps an inline preview and writes full text / omitted images to per-reply files. Empty input remains normal polling.
- Session-scoped overflow settings are available in R via `options(mcp.reply_overflow.* = ...)`. Supported keys are `behavior`, `text.preview_bytes`, `text.spill_bytes`, `images.preview_count`, `images.spill_count`, and `retention.max_dirs`.
- Pager mode is opt-in with `reply_overflow.behavior = "pager"` (or `options(mcp.reply_overflow.behavior = "pager")`). In pager mode, empty input advances one page. Non-empty pager commands must start with `:`. Non-`:` input dismisses pager and is sent to the backend.
- Documentation entry points work in-band. Prefer the normal R interfaces such as `?topic`, `help()`, `vignette()`, and `RShowDoc("R-exts")`; the REPL renders their text/HTML output directly instead of launching an external viewer.
- For large manuals and help pages, pager mode can still be useful. `?topic`, `help()`, `vignette()`, and `RShowDoc()` can all open there. Use `:help` for commands. The main search flow is `:/pattern`, `:n`, `:p`, `:matches`, and `:goto N`.
- Debugging: `browser()`, `debug()`, `trace()`.
- Control: `\u0003` in input interrupts; `\u0004` resets session then runs remaining input.
