# REPL Unread Output Redesign

Date: 2026-03-13
Status: Proposed

## Summary

Replace the current ring-buffer and post-hoc overflow reconstruction logic with a server-owned unread-output sink.

The server will continue draining worker stdout and stderr eagerly at all times using the existing dedicated reader threads. Instead of retaining a replayable transcript and inferring truncation or lifecycle state later, the server will keep only unread output that has not yet been shown. Each `repl(...)` call will wait until the REPL becomes idle or the timeout expires, then drain the unread batch exactly once and format that drained batch for the MCP client.

Small drained batches will be returned inline. Oversized drained batches will return an inline head-and-tail preview plus a retained overflow file containing the complete batch for that one reply. That overflow file is a convenience artifact and not part of live unread-output storage.

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
- Keep interrupt/restart control-byte prefixes and their compound forms as valid input forms.
- Make overflow files self-contained and meaningful for one returned reply only.
- Retain recent overflow files as a convenience for roughly the last 10-20 replies.

## Non-Goals

- Preserve a full replayable per-job transcript in normal operation.
- Support multiple simultaneously running jobs within one REPL session.
- Introduce input queueing while the worker is busy.
- Make overflow retention part of reply correctness after the reply has been delivered.

## Chosen Approach

Use one unread-only `PendingOutput` owner per server-side REPL wrapper.

`PendingOutput` starts in memory and stays active independently of any active tool call and independently of a particular worker process lifetime. If unread output grows too large before the next drain, it promotes once to an internal spill representation on disk. When a `repl(...)` call returns, it drains all unread output exactly once and removes it from in-memory or on-disk storage. If the drained batch itself is oversized for inline presentation, the server creates a separate retained overflow artifact for that returned reply.

This keeps capture, unread state, and drain semantics in one place while keeping reply overflow retention as a separate convenience layer.

## User-Facing Semantics

### Session Model

Each REPL session has at most one current worker and one execution state, but unread output is owned by the server-side REPL wrapper rather than by a particular worker lifetime:

- `IdleNoUnread`
- `IdleWithUnread`
- `Busy`
- `CaptureFailed`

`PendingOutput` belongs to the server-side REPL wrapper, not to the currently running tool call and not to a single worker lifetime. This is required because the worker or a child process may continue writing to stdout/stderr after a previous tool call has already returned and the session is otherwise idle, and because one returned reply may legitimately include output from before a worker exit and after the next worker respawn.

There is no input queue. Plain non-empty input submitted while `Busy` is active is rejected unless the input begins with a control prefix that explicitly interrupts or restarts the session.

Requests for one session are serialized by the server. At most one `repl` or `repl_reset` request may be actively waiting or draining for a session at a time.

### Input Forms

The server parses each request into:

- optional control action: none / interrupt / restart
- optional code payload

The exact control-byte contract remains the existing one already documented in the `repl` tool descriptions:

- `\u0003` in input interrupts
- `\u0004` in input resets the session and then runs the remaining input

Those escape sequences are documentation notation for the control bytes. This spec does not redefine them.

Valid examples:

- `1 + 1`
- `""`
- `\u0003`
- `\u0004`
- `\u00031 + 1`
- `\u00041 + 1`

### Request Semantics

#### `repl(code, timeout=T)` with plain non-empty `code`

- If the session is `Busy`, reject the request.
- If the session is `IdleNoUnread` or `IdleWithUnread`, start executing `code`.
- Wait until the session becomes idle or until `timeout` expires.
- Drain unread output once and return that drained batch.

If unread session output already exists before the new command starts, that unread prefix is preserved and included in the returned batch before the echoed input and the new command output. This preserves the current repo behavior where background output that arrived between tool calls is surfaced on the next call.

#### `repl("", timeout=T)`

- Do not start a new command.
- Wait until the session becomes idle or until `timeout` expires.
- Drain unread output once and return that drained batch.

This is the only supported read path for a timed-out or still-running job.

If the session is already idle and there is unread output, return it immediately.

If the session is already idle and there is no unread output, preserve the current poll behavior by returning the existing idle-status marker and prompt hint rather than inventing a new meta protocol.

#### `repl("\u0003", timeout=T)`

- Interrupt the current busy execution if one exists.
- Wait until the session becomes idle or until `timeout` expires.
- Drain unread output once and return that drained batch.

Unread output produced before the interrupt is preserved and included.

#### `repl("\u0004", timeout=T)`

- Restart the session.
- Wait until the session becomes idle or until `timeout` expires.
- Drain unread output once and return that drained batch.

Unread output produced before the restart is preserved and included.

#### `repl("\u0003" + code, timeout=T)` and `repl("\u0004" + code, timeout=T)`

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

### Reply Outcome Signaling

This redesign does not add a new machine-readable MCP status field. It preserves the current repo-visible reply markers:

- timed-out but still-busy replies include `<<console status: busy, ...>>`
- idle poll replies with no unread output include `<<console status: idle>>`
- restart replies continue to include `[repl] new session started`

These markers remain part of the returned batch content so existing transcript-style clients keep working during the redesign.

## Compatibility Decisions

The redesign preserves these current public behaviors:

- background output collected between tool calls is surfaced as a prefix in the next returned batch
- explicit input echo behavior when prefix/background output needs attribution
- timeout status marker text
- idle poll status marker text
- restart notice text
- prompt stripping and prompt re-append behavior
- current internal `promptVariants`-driven prompt cleanup behavior
- plot/image update collapsing within one returned batch
- the separate `repl_reset` tool
- current session-end / respawn behavior, including session-ended output being surfaced in-band and the next plain request being allowed to spawn a fresh worker session

The redesign intentionally changes or retires these behaviors:

- unread output ownership is server-level rather than tied to an active request or a single worker lifetime
- there is no replayable full-job transcript in normal operation
- overflow artifacts represent one returned reply batch only, never a whole job transcript
- `overflowResponseToken` will stop being emitted
- `codex/overflow-response-consumed` remains accepted as a no-op compatibility notification during the transition and can be removed later

## Architecture

### 1. Worker Output Readers

Keep the existing dedicated reader threads for worker stdout and stderr. These threads continue to:

- block on reading worker output
- forward text chunks into the server-owned unread-output sink immediately

Image events continue to be forwarded into the same sink through the existing server-side integration point.

This layer should not know anything about timeout policy, reply paging, or overflow retention.

### 2. Active Execution State

Introduce an explicit execution-state owner for whether the session is currently busy.

Responsibilities:

- expose `wait_until_idle_or_deadline(deadline)`
- expose whether the session is currently `Busy`
- expose whether capture has entered the latched `CaptureFailed` state

This unit does not own unread output. It only owns execution/busy state.

It also owns session-lifecycle state:

- whether the worker is alive
- whether the previous session ended cleanly
- whether the next plain request should spawn a fresh worker process

### 3. PendingOutput

`PendingOutput` is the canonical unread-output store for the server-side REPL wrapper.

It contains only output that has been captured from the worker or its descendants and not yet shown to the MCP client.

Once output has been returned to the client, it is removed from `PendingOutput` and is no longer kept in memory or spill storage.

`PendingOutput` must survive worker exit and respawn. A later returned batch may contain unread output captured before a worker exit and unread output captured after a fresh worker has been spawned, as long as neither portion has been shown yet.

#### States

- `InMemory`
- `SpilledToDisk`

Promotion is one-way for the life of the session until unread output has been fully drained and the spill representation can be cleared. Spill state is about unread output volume, not about whether a specific tool call is active.

#### Stored Items

Unread output is stored as ordered items:

- stdout text
- stderr text
- image event

Ordering is the order in which the server received the events from the worker integration points.

Before reply formatting, superseded plot/image updates inside the same drained batch continue to be collapsed using the repo's current `is_new` grouping behavior. The unread sink stores raw ordered image events; the batch formatter preserves current public behavior by applying the collapse step just before presentation.

### 4. Internal Spill Storage

Internal spill storage is for unread output only. It is not user-facing and is not retained after the unread batch has been drained.

This spill layer exists only to support a running job that produces more unread output than the in-memory budget before the next reply is returned.

Recommended shape:

- one server-owned temporary directory for unread spill storage
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
- if the batch exceeds inline limits, return an inline head-and-tail preview plus an overflow file containing the complete drained batch for this reply
- overflow decisions and preview sizing must be based on one total inline-size budget for the whole reply preview, measured in characters or bytes chosen during planning, not on line count
- image ordering must be preserved
- current prompt cleanup behavior must be preserved
- current input-echo behavior must be preserved
- current timeout / idle / restart marker text must be preserved
- the inline preview must consist of two non-overlapping slices from the drained batch: a head slice and a tail slice
- the inline preview must insert a single synthetic truncation notice between those slices, and that notice must make it explicit that the middle of the current reply was omitted from inline presentation by the REPL
- synthetic truncation notices must use the REPL's own marker style so they are distinguishable from real program output
- the preview should prefer stable item boundaries such as complete text lines and complete image-path notices when that fits within the budget
- if no such boundary fits because a single text line is itself too large, the preview must still honor the total inline-size budget by cutting within that text line rather than expanding the reply
- partial image-path notices must never be shown inline; if an image-path notice does not fit as a whole, omit it from the preview and leave it available only in the full reply artifact
- the overflow file must be self-contained and include the same head and tail content that appeared in the preview

The wording should refer to the current reply, not to the full job. For example:

`[repl] middle of this reply omitted from inline preview; full reply at ...`

The exact head-vs-tail budget split can be chosen during planning, but the default should favor the head slightly while always leaving room for a meaningful tail. That split still operates within one total inline-size budget rather than a line-count quota.

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

### Background Output While Idle

1. a command returns and the session becomes idle
2. later, the worker process or one of its children emits more stdout/stderr
3. reader threads append that output into server-owned `PendingOutput`
4. a later `repl("")` or plain `repl(code, ...)` drains that unread prefix exactly once

This is a required behavior of the redesign, not an edge-case fallback.

### Long-Running Job With Polls

1. request starts a command and times out before idle
2. unread output remains in `PendingOutput`
3. later `repl("")` waits until idle or timeout
4. handler drains only the unread output accumulated since the last returned batch
5. returned batch never overlaps with earlier replies

If the model polls often enough and each drained batch is small, no files are created.

### Worker Exit While Idle Or Busy

1. the worker exits cleanly, crashes, or the IPC/session reaches EOF
2. any final output observed by the always-on reader threads is appended into server-owned `PendingOutput`
3. the execution-state controller marks the session as ended and not busy
4. the next drain surfaces the session-ended output in-band using the same public behavior this repo already exposes today
5. the following plain request is allowed to spawn a fresh worker session

The redesign preserves the existing user-facing expectation that session end is surfaced in-band rather than hidden in out-of-band metadata. If unread output still exists when the fresh worker later emits new output, both portions may appear together in one later returned batch.

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
3. formatter returns a head-and-tail preview inline
4. formatter writes a retained overflow artifact containing the complete drained batch for this reply
5. retention manager keeps that artifact around for recent replies

That overflow artifact is for this reply only. It is not a full transcript for the whole job.

## Error Handling

### Busy Rejection

Plain non-empty input while `Busy` is active is rejected immediately.

### Interrupt / Restart

Interrupt and restart are explicit control actions. They do not discard unread output from the prior job.

### `repl_reset`

The separate `repl_reset` tool is preserved. It is semantically equivalent to a standalone leading `\u0004` request with no trailing code payload:

- preserve unread prefix output
- restart the session
- wait for idle or timeout
- drain one reply batch

### Internal Spill Failure

If promotion to internal spill storage fails, the session enters a latched `CaptureFailed` state clearly rather than silently dropping output.

In `CaptureFailed`:

- the next plain `repl(code, ...)` or `repl("", ...)` request returns a deterministic error batch describing the capture failure and does not execute new user code
- explicit restart (`repl_reset` or a leading `\u0004` request) is still allowed and is the recovery path

Implementation should prefer a small number of explicit failures over hidden fallback chains. The invariant is:

- do not advertise output as complete if spill/persistence failed

### Overflow Artifact Write Failure

If writing a retained overflow artifact fails, the reply stays bounded and lossy.

Required behavior:

- the drained batch has already been removed from unread storage, so the server must not rely on re-reading it later
- the server still returns only the normal bounded inline response for that reply, not the full drained batch
- that bounded inline response continues to use the same head-and-tail preview shape
- the bounded inline response must include a short notice that overflow persistence failed because the server could not write the artifact
- any undisplayed tail from that drained batch is dropped

This is a rare presentation failure, not a capture failure. It must not duplicate output or flood the client context with an oversized inline fallback.

### Worker Exit / Respawn

The redesign preserves the current model where worker exit is not automatically fatal to the whole session abstraction.

Required behavior:

- if the worker exits or the session ends, the session leaves `Busy` and becomes idle
- any unread output already captured before the exit remains drainable exactly once
- a subsequent plain `repl(code, ...)` request is allowed to spawn a fresh worker session unless the session is in the latched `CaptureFailed` state
- `repl_reset` also remains a valid explicit respawn path

The implementation plan should keep the current in-band session-end notices and restart notices consistent with today's public transcripts.

## Testing Strategy

Test through the public `repl(...)` API and transport behavior.

Required coverage:

- small inline reply from idle command
- background output arriving while no tool call is active
- plain input while idle with unread prefix output
- timed-out command followed by `repl("")` poll
- repeated polls that return non-overlapping batches
- `repl("", timeout=T)` waiting until idle rather than returning early due to already-buffered unread output
- leading `\u0003` preserving unread pre-interrupt output
- leading `\u0004` preserving unread pre-restart output
- `\u0003 + code` and `\u0004 + code` returning one combined batch across both phases
- `\u0003 + code` and `\u0004 + code` skipping the new code payload if phase 1 never reaches idle before the deadline
- idle poll with no unread output still returning the current idle marker behavior
- `repl_reset` matching standalone restart semantics
- plot/image update collapsing within one returned batch
- internal spill promotion for long-running unread output
- latched `CaptureFailed` behavior when spill promotion fails without an active request in flight
- oversized drained batch creating a self-contained overflow artifact for that reply only
- oversized drained batch preview showing both a non-overlapping head slice and tail slice with one explicit middle-omission notice
- overflow decisions being driven by total inline-size budget rather than line count
- very long single-line text still overflowing and previewing correctly within the total inline-size budget
- many short lines overflowing based on total size rather than number of lines
- preview truncation preferring complete lines and complete image-path notices when they fit
- preview truncation cutting within a single oversized text line when needed to stay within budget
- preview truncation never exposing a partial image-path notice
- overflow artifact write failure returning the normal bounded inline response plus a short write-failure notice
- multiple oversized polls producing separate overflow artifacts with no overlap
- retention window eviction for old overflow artifacts
- missing older overflow files simply disappearing after eviction
- legacy `codex/overflow-response-consumed` notification being accepted as a no-op during compatibility
- worker exit while busy still leaving already-captured unread output drainable once
- worker exit while idle preserving current in-band session-end behavior
- next plain request after worker exit spawning a fresh worker session

Regression tests should avoid asserting internal capture implementation details such as ring offsets or transport-hook bookkeeping, because those are intentionally being removed.

## Implementation Notes

This redesign should remove rather than layer on top of the current machinery.

Expected simplifications:

- remove ring-based unread tracking
- remove truncation inference based on replay gaps and synthetic notice events
- remove transport-coupled overflow-file liveness bookkeeping
- remove any meaning of `full response` that spans more than one returned reply batch

The implementation plan should prefer a small number of well-bounded units:

- request parser for control-byte prefixes
- session execution-state controller
- server-owned pending unread-output sink
- reply batch formatter
- overflow artifact retention manager

Each unit should have one clear responsibility and a narrow interface.

## Open Questions Deferred To Planning

- the exact in-memory unread-output budget before internal spill promotion
- the exact retained-reply count and coarse bytes cap defaults
- the exact on-disk layout for internal spill storage
- whether the spill implementation is best expressed as one append-only text file plus index, or as ordered spill segments

These do not change the external semantics defined above.
