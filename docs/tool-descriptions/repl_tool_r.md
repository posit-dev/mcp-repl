The r repl tool executes R code in a persistent session. Returns stdout, stderr, and rendered plots.

Arguments:
- `input` (string): R code to execute. Send empty string to poll for output from long-running work.
- `timeout_ms` (number, optional): Max milliseconds to wait (bounds this call only; doesn't cancel backend work).

Behavior:
- Session state (variables, loaded packages) persists across calls. Errors don't crash the session.
- Uses the user's R installation and library paths.

- Plots (ggplot2 and base R) are captured and returned as images. Adjust sizing with `options(console.plot.width, console.plot.height, console.plot.units, console.plot.dpi)`.
- Pager mode activates on large output. All pager commands start with `:` (for example `:q`, `:/pattern`, `:n`); any input not prefixed with `:` automatically dismisses pager and is sent to the backend.
- Help via `?topic` or `help()` opens a pager. Use `:q` to exit, `:n` for next page, `:/pattern` to search.
- Debugging: `browser()`, `debug()`, `trace()`.
- Control: `\u0003` in input interrupts; `\u0004` resets session then runs remaining input.
