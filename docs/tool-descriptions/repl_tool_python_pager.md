`repl` runs source text in a persistent Python session and returns emitted stdout/stderr and images.

Arguments:
- `input` (string): Python source text to run in the persistent session, or stdin text when Python is already waiting for input. Send empty input while the pager is active to advance one page.
- `timeout_ms` (number, optional): maximum milliseconds to wait before returning.
  Timeout bounds only this response window; it does not cancel backend work.

Python REPL affordances:
- Session state persists across calls; treat persistence as an iteration aid, not a correctness guarantee.
- Non-empty input while the session is idle runs as one complete Python cell with persistent globals. Send multi-line blocks and following top-level code in the same call.
- A final top-level expression is displayed through `sys.displayhook`, so custom display hooks are honored.
- Incomplete code such as a bare block header reports a normal Python syntax error instead of entering continuation mode.
- If running code asks for stdin through `input()`, `help()`, `pdb`, `sys.stdin`, or raw stdin APIs, the next non-empty `input` is delivered as stdin bytes for that running code. It is not executed as new Python source.
- Do not mix code plus buffered stdin answers in one payload. Send stdin answers only after Python has reported that it is waiting for input.
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
