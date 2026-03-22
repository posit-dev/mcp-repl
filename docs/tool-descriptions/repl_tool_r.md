The r repl tool executes R code in a persistent session. Returns stdout, stderr, and rendered plots.

Arguments:
- `input` (string): R code to execute. Send empty string to poll for output from long-running work.
- `timeout_ms` (number, optional): Max milliseconds to wait (bounds this call only; doesn't cancel backend work).

Behavior:
- Session state (variables, loaded packages) persists across calls. Errors don't crash the session.
- Uses the user's R installation and library paths.

- Plots (ggplot2 and base R) are captured and returned as images. Adjust sizing with `options(console.plot.width, console.plot.height, console.plot.units, console.plot.dpi)`.
- Empty `input` polls for more output from a timed-out request or for detached background output while idle.
- If a request times out, keep polling with empty `input` until the remaining worker output is drained. New non-empty input is discarded while that timed-out request is still active.
- Large output replies may be written to a server-owned output bundle. The inline reply stays bounded and may show only a preview plus an absolute `events.log` path for the ordered output bundle.
- Output bundles contain `transcript.txt`, `events.log`, and `images/`, even for text-only oversized replies.
- `transcript.txt` contains worker-originated REPL text such as echoed input, prompts, stdout, and rendered stderr text. Server status lines stay inline and are not written into `transcript.txt`.
- `events.log` is the authoritative ordered index for the retained bundle contents. `T` rows point to line and byte ranges in `transcript.txt`. `I` rows point to relative image paths under `images/`. If bundle retention limits omit tail content, both the inline reply and `events.log` report that omission.
- When an output bundle is used for images, the inline preview keeps the first and last image as anchors. Inspect `events.log`, then open the needed transcript ranges or numbered image files.
- Older output bundles may be pruned to keep storage bounded. A disclosed bundle path remains usable until it is pruned or the server exits.
- Documentation entry points work in-band. Prefer the normal R interfaces such as `?topic`, `help()`, `vignette()`, and `RShowDoc("R-exts")`; the REPL renders their text/HTML output directly instead of launching an external viewer.
- `?topic`, `help()`, `vignette()`, and `RShowDoc()` render directly into the tool response instead of opening a pager.
- Debugging: `browser()`, `debug()`, `trace()`.
- Control: `\u0003` in input interrupts; `\u0004` resets session then runs remaining input.
