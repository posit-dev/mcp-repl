# Advisory Worker Write Observations

## Summary

If `mcp-repl` continues to own some embedded-runtime write callbacks, those
callbacks could emit advisory IPC metadata about worker-owned writes.

The important constraint is that these observations must not replace pipe
capture as the source of truth for visible output. They are only additional
facts the server can use opportunistically.

## Motivation

Today, worker-owned writes such as embedded R console callbacks know more than
just "readline happened":

- they know which stream is being written,
- they know the exact byte slice being written,
- they know the local callback order relative to other worker-side events such
  as `readline_result` and `plot_image`.

That information is incomplete, but it may still be useful.

Possible benefits:

- better ordering corrections for worker-owned writes relative to images and
  readline events,
- detection of mismatches between callback-observed writes and what later
  appears on the pipe,
- visibility into same-process write contention or corruption when multiple
  writers race on the worker's stdio.

## Important Limitation

These observations are inherently partial.

They do not describe output from:

- child processes,
- forked processes,
- background processes inheriting stdio,
- any writer that bypasses the embedded-runtime callback path.

So they must remain advisory metadata, not a complete output timeline.

## Intended Direction

- Keep stdout/stderr pipe capture as the authoritative visible-output source.
- If worker-owned callbacks remain, optionally emit IPC write observations for
  those callbacks.
- Use those observations only for best-effort ordering and diagnostics.
- Never treat them as request-completion signals.
- Never assume they cover subprocess or descendant output.

In short: "callback saw a write" is a useful hint, not the truth.

## Possible Event Shape

If this is prototyped, keep it narrow:

- stream: stdout or stderr
- byte length
- small preview and/or checksum
- worker-local monotonic sequence number
- whether the write came from embedded runtime output, echoed input, or another
  worker-owned path

The event should be cheap to emit and easy to ignore when it is not helpful.

## Relationship To Other Work

This is separate from `docs/futurework/r-embedding-minimal-callbacks.md`.

- minimal callbacks asks whether the worker should own fewer write callbacks
  overall
- advisory write observations ask what extra metadata, if any, is worth
  emitting from the callbacks that remain

Those directions can coexist, but neither implies the other.

## Non-Goals

- Replacing stdout/stderr pipe capture
- Solving subprocess or forked-process output attribution
- Using write observations as request-completion boundaries
- Expanding the current timeline fix into a broad IPC protocol redesign
