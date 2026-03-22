`repl` runs source text in a persistent Python REPL session and returns emitted stdout/stderr and images.

Arguments:
- `input` (string): bytes to write to backend stdin.
- `timeout_ms` (number, optional): maximum milliseconds to wait before returning.
  Timeout bounds only this response window; it does not cancel backend work.

Python REPL affordances:
- Session state persists across calls; treat persistence as an iteration aid, not a correctness guarantee.
- While work is still running, concurrent non-empty input is discarded; use empty `input` to poll.
- Empty `input` polls for more output from a timed-out request or for detached background output while idle.
- If a request times out, keep polling with empty `input` until the remaining worker output is drained. New non-empty input is discarded while that timed-out request is still active.
- Large output replies may be written to a server-owned output bundle. The inline reply stays bounded and may show only a preview plus an absolute `events.log` path for the ordered output bundle.
- Output bundles contain `transcript.txt`, `events.log`, and `images/`.
- `transcript.txt` contains worker-originated REPL text such as echoed input, prompts, stdout, and rendered stderr text. Server status lines stay inline and are not written into `transcript.txt`.
- `events.log` is the authoritative ordered index for the full reply stream. `T` rows point to line and byte ranges in `transcript.txt`. `I` rows point to relative image paths under `images/`.
- When an output bundle is used for images, the inline preview keeps the first and last image as anchors. Inspect `events.log`, then open the needed transcript ranges or numbered image files.
- Plot images are returned as image content (for example matplotlib output).
- Help flows are in-band (`help()`, `dir()`, `pydoc.help`).
- Debugging workflows are supported (`breakpoint()`, `pdb.set_trace()`).
- Control prefixes in `input`: `\u0003` (interrupt) and `\u0004` (reset then run remaining input).
