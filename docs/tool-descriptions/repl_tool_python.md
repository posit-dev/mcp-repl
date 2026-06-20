`repl` runs source text in a persistent Python REPL session and returns emitted stdout/stderr and images.

Arguments:
- `input` (string): Python source text to send to the persistent REPL session.
- `timeout_ms` (number, optional): maximum milliseconds to wait before returning.
  Timeout bounds only this response window; it does not cancel backend work.

Python REPL affordances:
- Session state persists across calls; treat persistence as an iteration aid, not a correctness guarantee.
- Input is sent to CPython interactive mode line by line, not script or notebook-cell mode; script-like blocks may execute earlier than they would in a file or cell.
- When a compound block may continue, end the submitted input with two newline characters (`\n\n`) or put `\n\n` before unrelated top-level code.
- If Python is waiting at the continuation prompt, send a blank line (`\n`) to run a complete block, or send `\u0003` to abandon pending input without resetting the session.
- While work is still running, concurrent non-empty input is discarded; use empty `input` to poll.
- Empty `input` polls for more output from a timed-out request or for detached background output while idle.
- If a request times out, keep polling with empty `input` until the remaining worker output is drained. New non-empty input is discarded while that timed-out request is still active.
- Large output replies may stay inline when only slightly oversized. Larger overages may be written to a server-owned output bundle directory. The inline reply stays bounded and may show a preview plus the most relevant disclosed path inside that bundle.
- Bundle files are materialized lazily. Text-only oversized replies disclose `transcript.txt`. Image bundles use `images/` for the latest image aliases and `images/history/` for ordered image history. `events.log` is created only once a bundle needs ordered mixed text+image indexing.
- `transcript.txt` contains worker-originated REPL text such as echoed input, prompts, stdout, and rendered stderr text. Ordinary server status lines stay inline and are not written into `transcript.txt`.
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
- Control prefixes in `input`: `\u0003` (interrupt) and `\u0004` (reset then run remaining input).
