`repl` runs source text in a persistent Python session and returns emitted stdout/stderr and images.

Arguments:
- `input` (string): Python source text to run in the persistent session, or stdin text when Python is already waiting for input.
- `timeout_ms` (number, optional): maximum milliseconds to wait before returning.
  Timeout bounds only this response window; it does not cancel backend work.

Python REPL affordances:
- No startup or initialization call is needed; the REPL is self-healing and starts ready for Python code.
- Session state persists across calls; treat persistence as an iteration aid, not a correctness guarantee.
- At the start of each non-empty call, Python routes the whole payload from current state: if it is waiting for stdin, the payload is stdin; otherwise the payload runs as one complete Python cell with persistent globals. Send multi-line blocks and following top-level code in the same cell call.
- A final top-level expression is displayed through `sys.displayhook`, so custom display hooks are honored.
- Incomplete code such as a bare block header reports a normal Python syntax error instead of entering continuation mode.
- If running code asks for stdin through `input()`, `help()`, `pdb`, `sys.stdin`, or raw stdin APIs, the next non-empty `input` is delivered as stdin bytes for that running code. The whole payload is stdin for that call, including additional lines.
- Send new Python source in a later call after Python is ready for cell execution; leftover stdin is not promoted to source at the server boundary.
- While work is still running, concurrent non-empty input is discarded; use empty `input` to poll.
- Empty `input` polls for more output from a timed-out request or for detached background output while idle.
- If a request times out, keep polling with empty `input` until the remaining worker output is drained. New non-empty input is discarded while that timed-out request is still active.
- Large output replies may stay inline when only slightly oversized. Larger overages may be written to a server-owned output bundle directory. The inline reply stays bounded and may show a preview plus the most relevant disclosed path inside that bundle.
- Files mode expects the model to have filesystem tools to read files and list directories.
- Bundle files are materialized lazily. Text-only oversized replies disclose `transcript.txt`. Image bundles use `images/` for the latest image aliases and `images/history/` for ordered image history. `events.log` is created only once a bundle needs ordered mixed text+image indexing.
- `transcript.txt` contains worker-originated REPL text such as prompts, stdout, and rendered stderr text. Ordinary server status lines stay inline and are not written into `transcript.txt`.
- `events.log`, when present, is the authoritative ordered index for the retained mixed worker-text/image bundle contents. `T` rows point to line and byte ranges in `transcript.txt`. `I` rows point to relative image history paths such as `images/history/001/002.png`. `S` rows are reserved for server-only omission notices when bundle retention drops later content.
- When an output bundle is used for images, the inline preview keeps the first and last image as anchors. Inspect top-level files under `images/` first for the latest image state. Use `events.log` plus `images/history/` only when you need ordered image history.
- Example image bundle layout:
  - `images/001.png`
  - `images/002.png`
  - `images/history/001/001.png`
  - `images/history/001/002.png`
  - `images/history/002/001.png`
- Older output bundles may be pruned to keep storage bounded. A disclosed bundle path remains usable until it is pruned or the server exits.
- Plot images are returned as image content (for example matplotlib output).
- Help flows are in-band (`help()`, `dir()`, `pydoc.help`).
- Debugging works in the REPL, including interactive stops from `breakpoint()` and `pdb.set_trace()`.
- Control prefixes in `input`: `\u0003` (interrupt) and `\u0004` (Ctrl-D / EOF restart). If input remains after `\u0004`, the same reply includes bounded-shutdown output followed by fresh-session output under the original call timeout.
