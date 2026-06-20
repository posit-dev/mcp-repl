`repl` runs source text in a persistent Python REPL session and returns emitted stdout/stderr and images.

Arguments:
- `input` (string): Python source text to send to the persistent REPL session. Send empty input while the pager is active to advance one page.
- `timeout_ms` (number, optional): maximum milliseconds to wait before returning.
  Timeout bounds only this response window; it does not cancel backend work.

Python REPL affordances:
- Session state persists across calls; treat persistence as an iteration aid, not a correctness guarantee.
- Input is sent to CPython interactive mode line by line, not script or notebook-cell mode; script-like blocks may execute earlier than they would in a file or cell.
- When a compound block may continue, end the submitted input with two newline characters (`\n\n`) or put `\n\n` before unrelated top-level code.
- If Python is waiting at the continuation prompt, send a blank line (`\n`) to run a complete block, or send `\u0003` to abandon pending input without resetting the session.
- While work is still running, concurrent non-empty input is discarded; use empty `input` to poll.
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
- Plot images are returned as image content (for example matplotlib output).
- Help flows are in-band (`help()`, `dir()`, `pydoc.help`).
- Debugging works in the REPL, including interactive stops from `breakpoint()` and `pdb.set_trace()`.
- Control prefixes in `input`: `\u0003` (interrupt) and `\u0004` (reset then run remaining input).
