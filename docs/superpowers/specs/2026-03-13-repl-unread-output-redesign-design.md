# REPL Unread Output Redesign

Date: 2026-03-13
Status: Proposed

## Summary

Replace the current ring-buffer and post-hoc overflow reconstruction logic with a per-job unread-output sink.

The server will continue draining worker stdout and stderr eagerly at all times using the existing dedicated reader threads. Instead of retaining a replayable transcript and inferring truncation or lifecycle state later, the server will keep only unread output for the active job. Each `repl(...)` call will wait until the REPL becomes idle or the timeout expires, then drain the unread batch exactly once and format that drained batch for the MCP client.

Small drained batches will be returned inline. Oversized drained batches will return an inline preview plus a retained overflow file containing the complete batch for that one reply. That overflow file is a convenience artifact and not part of live unread-output storage.

## Problem

The current branch spread one feature across three different responsibilities:

- output capture and truncation detection
- reply formatting and overflow persistence
- overflow artifact lifetime and delivery timing

That led to repeated correctness bugs around:

- false or missing `full response` links
- dropped image references
- same-response artifacts being evicted too early
- transport-delivery races
- truncation being detected indirectly instead of represented directly

The core issue is architectural. The implementation currently reconstructs persisted replies after capture instead of treating a reply as a first-class owned output stream.

## Goals

- Keep worker pipes drained eagerly so the worker is never blocked writing output.
- Ensure the MCP client never receives duplicate content.
- Make each `repl(...)` return a self-contained, non-overlapping output batch.
- Keep small and quick replies purely in memory.
- Support long-running jobs with repeated polling via `repl("")`.
- Keep `^C`, `^D`, `^Ccode`, and `^Dcode` as valid input forms.
- Make overflow files self-contained and meaningful for one returned reply only.
- Retain recent overflow files as a convenience for roughly the last 10-20 replies.

## Non-Goals

- Preserve a full replayable per-job transcript in normal operation.
- Support multiple simultaneously running jobs within one REPL session.
- Introduce input queueing while the worker is busy.
- Make overflow retention part of reply correctness after the reply has been delivered.

## Chosen Approach

Use one unread-only `PendingOutput` owner per active job.

`PendingOutput` starts in memory. If unread output grows too large before the next drain, it promotes once to an internal spill representation on disk. When a `repl(...)` call returns, it drains all unread output exactly once and removes it from in-memory or on-disk storage. If the drained batch itself is oversized for inline presentation, the server creates a separate retained overflow artifact for that returned reply.

This keeps capture, unread state, and drain semantics in one place while keeping reply overflow retention as a separate convenience layer.

## User-Facing Semantics

### Session Model

Each REPL session has exactly one worker and one execution state:

- `Idle`
- `Busy(job)`

There is no input queue. Non-empty input submitted while `Busy(job)` is active is rejected unless the input begins with a control prefix that explicitly interrupts or restarts the session.

### Input Forms

The server parses each request into:

- optional control action: none / interrupt (`^C`) / restart (`^D`)
- optional code payload

Valid examples:

- `1 + 1`
- `""`
- `^C`
- `^D`
- `^C1 + 1`
- `^D1 + 1`

### Request Semantics

#### `repl(code, timeout=T)` with plain non-empty `code`

- If the session is `Idle`, start executing `code`.
- If the session is `Busy(job)`, reject the request.
- Wait until the session becomes idle or until `timeout` expires.
- Drain unread output once and return that drained batch.

#### `repl("", timeout=T)`

- Do not start a new command.
- Wait until the session becomes idle or until `timeout` expires.
- Drain unread output once and return that drained batch.

This is the only supported read path for a timed-out or still-running job.
If the session is already idle and there is no unread output, return an empty batch immediately.

#### `repl("^C", timeout=T)`

- Interrupt the active job if one exists.
- Wait until the session becomes idle or until `timeout` expires.
- Drain unread output once and return that drained batch.

Unread output produced before the interrupt is preserved and included.

#### `repl("^D", timeout=T)`

- Restart the session.
- Wait until the session becomes idle or until `timeout` expires.
- Drain unread output once and return that drained batch.

Unread output produced before the restart is preserved and included.

#### `repl("^Ccode", timeout=T)` and `repl("^Dcode", timeout=T)`

These are compound turns:

1. apply the control action
2. wait for the session to become idle
3. start `code`
4. wait again until the session becomes idle or until `timeout` expires
5. drain unread output once and return one combined batch

Unread output from the replaced job is preserved and may appear before the new command's output in the returned batch.

These compound turns use one overall deadline for the full request. The timeout budget is not reset between phases.

If the control-action phase does not reach idle before the deadline expires, the server returns the unread output accumulated so far and does not start the new `code` payload.

### Timeout Semantics

`timeout` does not control whether worker pipes are drained. The server always drains worker pipes eagerly in the background.

`timeout` only controls how long the `repl(...)` call waits before it snapshots and drains unread output for the client.

`timeout=0` does not need a separate implementation path. It is just an already-expired deadline.

## Architecture

### 1. Worker Output Readers

Keep the existing dedicated reader threads for worker stdout and stderr. These threads continue to:

- block on reading worker output
- forward text chunks into the active job output sink immediately

Image events continue to be forwarded into the same sink through the existing server-side integration point.

This layer should not know anything about timeout policy, reply paging, or overflow retention.

### 2. Active Job

Introduce an explicit `ActiveJob` owner for the current busy execution.

Responsibilities:

- own the current `PendingOutput`
- receive text and image events in order
- expose `wait_until_idle_or_deadline(deadline)`
- expose `drain_unread_batch()`
- expose completion / interrupted / restarted status to the request handler

There is at most one `ActiveJob` per session.

### 3. PendingOutput

`PendingOutput` is the canonical unread-output store for the active job.

It contains only output that has been captured from the worker but not yet shown to the MCP client.

Once output has been returned to the client, it is removed from `PendingOutput` and is no longer kept in memory or spill storage.

#### States

- `InMemory`
- `SpilledToDisk`

Promotion is one-way for the life of a job. Once a job spills to disk, unread output for that job continues to use the spill representation until the job ends.

#### Stored Items

Unread output is stored as ordered items:

- stdout text
- stderr text
- image event

Ordering is the order in which the server received the events from the worker integration points.

### 4. Internal Spill Storage

Internal spill storage is for unread output only. It is not user-facing and is not retained after the unread batch has been drained.

This spill layer exists only to support a running job that produces more unread output than the in-memory budget before the next reply is returned.

Recommended shape:

- one per-job temporary directory
- one append-only text file for unread text
- image files for unread image items
- a small ordered metadata index if needed to reconstruct text/image ordering during drain

The exact on-disk layout is an implementation detail. The required behavior is:

- append unread items cheaply
- drain unread items once in order
- delete or truncate drained content immediately

### 5. Reply Batch Formatter

After `drain_unread_batch()`, the server formats the drained batch for one MCP reply.

This formatter decides only how to present the batch that was just drained. It does not read any older already-shown output.

Rules:

- if the batch fits inline limits, return it inline
- if the batch exceeds inline limits, return an inline preview plus an overflow file containing the complete drained batch for this reply
- image ordering must be preserved
- the overflow file must be self-contained and include the same head that appeared in the preview

The wording should refer to the current reply, not to the full job. For example:

`[repl] output for this reply truncated; full reply at ...`

### 6. Overflow Artifact Retention

Overflow artifacts are separate from internal spill storage.

They exist only when a drained reply batch is too large for inline presentation. They are retained as a convenience for recent replies and are not part of live unread-output capture.

Recommended policy:

- retain the last `N` oversized replies, defaulting to something like `16`
- also enforce a coarse total-bytes cap
- evict oldest retained reply artifacts first
- eviction is allowed to delete the files completely with no tombstone

Each retained reply artifact must be self-contained:

- one text file containing the full reply batch
- any image files referenced by that text file

## Data Flow

### Small Quick Reply

1. request starts a command
2. worker readers append unread output into `PendingOutput::InMemory`
3. command becomes idle before timeout
4. request handler drains unread output
5. formatter returns inline content
6. drained output is gone

No files are created.

### Long-Running Job With Polls

1. request starts a command and times out before idle
2. unread output remains in `PendingOutput`
3. later `repl("")` waits until idle or timeout
4. handler drains only the unread output accumulated since the last returned batch
5. returned batch never overlaps with earlier replies

If the model polls often enough and each drained batch is small, no files are created.

### Running Job With Internal Spill

1. job keeps producing output while unread output remains undrained
2. unread output exceeds the in-memory budget
3. `PendingOutput` promotes to internal spill storage
4. later `repl("")` drains unread output from the spill store once
5. drained spill content is removed immediately

This spill representation is invisible to the MCP client.

### Oversized Returned Reply

1. handler drains one unread batch
2. formatter determines that the batch is too large for inline presentation
3. formatter returns a preview inline
4. formatter writes a retained overflow artifact containing the complete drained batch for this reply
5. retention manager keeps that artifact around for recent replies

That overflow artifact is for this reply only. It is not a full transcript for the whole job.

## Error Handling

### Busy Rejection

Plain non-empty input while `Busy(job)` is active is rejected immediately.

### Interrupt / Restart

Interrupt and restart are explicit control actions. They do not discard unread output from the prior job.

### Internal Spill Failure

If promotion to internal spill storage fails, fail the current request path clearly rather than silently dropping output.

Implementation should prefer a small number of explicit failures over hidden fallback chains. The exact user-facing message can be specified during implementation planning, but the invariant is:

- do not advertise output as complete if spill/persistence failed

### Overflow Artifact Write Failure

If writing a retained overflow artifact fails, the reply should still return the inline preview plus a clear message that the full reply could not be persisted.

This is a presentation failure, not a capture failure. It must not corrupt or duplicate unread output semantics.

## Testing Strategy

Test through the public `repl(...)` API and transport behavior.

Required coverage:

- small inline reply from idle command
- timed-out command followed by `repl("")` poll
- repeated polls that return non-overlapping batches
- `repl("", timeout=T)` waiting until idle rather than returning early due to already-buffered unread output
- `^C` preserving unread pre-interrupt output
- `^D` preserving unread pre-restart output
- `^Ccode` and `^Dcode` returning one combined batch across both phases
- internal spill promotion for long-running unread output
- oversized drained batch creating a self-contained overflow artifact for that reply only
- multiple oversized polls producing separate overflow artifacts with no overlap
- retention window eviction for old overflow artifacts
- missing older overflow files simply disappearing after eviction

Regression tests should avoid asserting internal capture implementation details such as ring offsets or transport-hook bookkeeping, because those are intentionally being removed.

## Implementation Notes

This redesign should remove rather than layer on top of the current machinery.

Expected simplifications:

- remove ring-based unread tracking
- remove truncation inference based on replay gaps and synthetic notice events
- remove transport-coupled overflow-file liveness bookkeeping
- remove any meaning of `full response` that spans more than one returned reply batch

The implementation plan should prefer a small number of well-bounded units:

- request parser for control prefixes
- active job / wait state controller
- pending unread-output sink
- reply batch formatter
- overflow artifact retention manager

Each unit should have one clear responsibility and a narrow interface.

## Open Questions Deferred To Planning

- the exact in-memory unread-output budget before internal spill promotion
- the exact retained-reply count and coarse bytes cap defaults
- the exact on-disk layout for internal spill storage
- whether the spill implementation is best expressed as one append-only text file plus index, or as ordered spill segments

These do not change the external semantics defined above.
