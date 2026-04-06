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
- Large output replies may stay inline when only slightly oversized. Larger overages may be written to a server-owned output bundle directory. The inline reply stays bounded and may show a preview plus the most relevant disclosed path inside that bundle.
- Bundle files are materialized lazily. Text-only oversized replies disclose `transcript.txt`. Image bundles use `images/` for the latest image aliases and `images/history/` for ordered image history. `events.log` is created only once a bundle needs ordered mixed text+image indexing.
- `transcript.txt` contains worker-originated REPL text such as echoed input, prompts, stdout, and rendered stderr text. Server status lines stay inline and are not written into `transcript.txt` or `events.log`.
- `events.log`, when present, is the authoritative ordered index for the retained mixed worker-text/image bundle contents. `T` rows point to line and byte ranges in `transcript.txt`. `I` rows point to relative image history paths such as `images/history/001/002.png`. If bundle retention limits omit tail content, the inline reply reports that omission, and mixed bundles also record it in `events.log`.
- When an output bundle is used for images, the inline preview keeps the first and last image as anchors. Inspect top-level files under `images/` first for the latest image state. Use `events.log` plus `images/history/` only when you need ordered image history.
- Example image bundle layout:
  - `images/001.png`
  - `images/002.png`
  - `images/history/001/001.png`
  - `images/history/001/002.png`
  - `images/history/002/001.png`
- Older output bundles may be pruned to keep storage bounded. A disclosed bundle path remains usable until it is pruned or the server exits.
- Documentation entry points work in-band. Prefer the normal R interfaces such as `?topic`, `help()`, `vignette()`, and `RShowDoc("R-exts")`; the REPL renders their text/HTML output directly instead of launching an external viewer.
- `?topic`, `help()`, `vignette()`, and `RShowDoc()` render directly into the tool response instead of opening a separate web-browser flow.
- Debugging works in the REPL, including interactive stops from `browser()`, `debug()`, and `trace()`.
- Control: `\u0003` in input interrupts; `\u0004` resets session then runs remaining input.
