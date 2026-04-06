The r repl tool executes R code in a persistent session. Returns stdout, stderr, and rendered plots.

Arguments:
- `input` (string): R code to execute. Send empty input while the pager is active to advance one page.
- `timeout_ms` (number, optional): Max milliseconds to wait (bounds this call only; doesn't cancel backend work).

Behavior:
- Session state (variables, loaded packages) persists across calls. Errors don't crash the session.
- Uses the user's R installation and library paths.
- Plots (ggplot2 and base R) are captured and returned as images. Adjust sizing with `options(console.plot.width, console.plot.height, console.plot.units, console.plot.dpi)`.
- Empty `input` polls for more output from a timed-out request or for detached background output while idle. While pager mode is active, empty input advances one page.
- If a request times out, keep polling with empty `input` until the remaining worker output is drained. New non-empty input is discarded while that timed-out request is still active.
- Oversized text output can enter a modal pager. While pager mode is active, backend input is blocked until you quit the pager or consume the remaining pages.
- Pager commands:
  - next page: empty input or `:next`
  - quit pager: `:q`
  - search: `:/pattern`
  - next/previous search hit: `:n`, `:p`
  - list matches / hits: `:matches`, `:hits`
  - help: `:help`
- Pager responses use `[pager]` status lines and may suppress the backend prompt until pager mode ends.
- Documentation entry points work in-band. Prefer the normal R interfaces such as `?topic`, `help()`, `vignette()`, and `RShowDoc("R-exts")`; the REPL renders their text/HTML output directly instead of launching an external viewer.
- `?topic`, `help()`, `vignette()`, and `RShowDoc()` render directly into the tool response instead of opening a separate web-browser flow.
- Debugging works in the REPL, including interactive stops from `browser()`, `debug()`, and `trace()`.
- Control: `\u0003` in input interrupts; `\u0004` resets session then runs remaining input.
