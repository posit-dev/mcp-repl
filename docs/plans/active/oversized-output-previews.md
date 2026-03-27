# Oversized Output Previews

## Summary

- Replace the remaining default pager-era large-output behavior with non-modal oversized text previews plus server-owned worker transcript files.
- Keep `PendingOutputTape` as the always-on raw event collector.
- Split the design across two independent axes:
  - reply-tracking state: what worker-originated text the server still owes to future `repl()` replies
  - file materialization: whether that worker-originated text is also being accumulated in a server-owned file
- Keep server-only notices out of worker transcript files.
- For a timed-out request, create a hidden worker transcript file immediately on the first timeout so the server does not need to retain unbounded text in memory.
- Only disclose a file path to the client when a response actually needs truncation or quarantine.
- Scope v1 to text-only oversized replies. Replies containing any image content remain unchanged.

## Status

- State: active
- Last updated: 2026-03-21
- Current phase: initial implementation shipped; follow-on cleanup still open
- Verification: `cargo check`, `cargo build`, `cargo clippy`, `cargo test`, and `cargo +nightly fmt` all pass in the implementation branch

## Current Direction

- Oversized-output logic happens only at reply finalization time after the tape snapshot is drained.
- Polling remains the primary interaction model.
- Detached idle output must never make `repl(input=...)` unusable.
- Worker transcript files are a fallback for omitted worker-originated text, not a mirror of the full visible reply.
- The shipped implementation creates a hidden worker transcript file on the first timeout reply, appends later worker-originated poll text to that same file, and discloses the path only if an oversized text-only reply is actually compacted.
- Non-timeout oversized text-only replies create a worker transcript file lazily at response time and disclose it immediately in the compacted reply.
- Mixed text+image replies stay unchanged in v1.
- Text-only spilling uses Unicode character count, not encoded byte length, because the threshold is meant to approximate visible reply size rather than bundle storage usage.
- Detached idle output remains non-blocking because `repl(input=...)` still accepts new input once no timed-out request is active; oversized text from that accepted reply follows the same text-only compaction path.

## Long-Term Direction

- The likely long-term direction is a more eager, state-machine-driven consumer of tape output rather than a pure seal-time formatter.
- The long-term design should preserve the same public behavior contract while reducing memory footprint and making output handling more incremental.
- The current seal-time phase exists to validate the public UX and worker-transcript contract first. It should not be treated as proof that the long-term implementation must remain seal-time.

## Phase Status

- Phase 0: completed. Default-path pager behavior was mostly removed, simplified, and isolated behind a separate legacy mode. `PendingOutputTape` became the main default collector.
- Phase 1: completed. Multiple design rounds narrowed the space: no modal pager, no default read tool, no live reader-thread formatting, and no worker-owned transcript files.
- Phase 2: completed. Lock the public text format, timeout follow-up behavior, worker-only file contents, and detached-idle behavior for text-only v1.
- Phase 3: active. Implement server-owned worker transcript storage, hidden-on-timeout file creation, and the seal-time formatter as the first bounded implementation step.
- Phase 4: pending. Revisit the implementation architecture after the public behavior is validated and move toward a more eager/state-machine-driven design.
- Phase 5: pending. Update tool descriptions and replace pager-oriented tests and snapshots for the default public surface.

## Reply-Tracking States

- `none`
  - no worker-originated text is owed to a future reply
- `timed-out request still owes worker text`
  - a prior non-empty `repl(input=...)` timed out
  - future empty-input polls continue surfacing worker-originated text for that same request
  - while this state is active, a new non-empty input is not accepted
- `detached idle worker text exists`
  - worker-originated text arrived while no request was active
  - this text may be surfaced on empty-input polls or as a bounded prefix/notice on a later accepted request
  - this state never blocks a new non-empty input

## File Materialization States

- `no file`
  - default for normal small replies
- `hidden worker transcript file`
  - the server is already writing worker-originated text to a file
  - the client has not yet been told the path
- `disclosed worker transcript file`
  - a response has already shown the path because truncation or quarantine happened

## Locked Decisions

- The tape always exists and always collects output, including output that arrives between tool calls.
- For the current phase, oversized-output logic happens only at reply finalization time after the tape snapshot is drained.
- Polling is the primary interaction model. Agents should keep using bounded `repl` replies and only inspect transcript files when needed.
- Worker transcript files are owned by the server, not the worker. They must survive worker death, segfault, restart, or reset within the same logical request or idle-output episode.
- Worker transcript files contain only worker-originated REPL text.
- Worker transcript files include prompts and echoed input exactly as surfaced from the worker-side interaction.
- Worker transcript files exclude server-only notices such as timeout markers, busy/rejection notices, and reset/session notices.
- For a timed-out request, create a hidden worker transcript file immediately on the first timeout reply.
- A hidden file path is disclosed only if a later response actually truncates or quarantines worker-originated text.
- If a file is first disclosed after earlier inline replies already showed worker text, it must already contain those earlier worker-originated bytes from the start of that same request or idle-output episode.
- Detached idle output never forces endless polling and never blocks a new non-empty input.
- If detached idle text is too large, compact or quarantine only that detached-idle portion and keep the new request usable.
- The inline reply uses one middle truncation marker line only. No separate metadata block.
- Line-based preview is the default public behavior. Char-based preview is fallback only when a clean line-aligned preview cannot fit the internal budget.
- In line mode and char mode, the marker reports what is already shown, not extra helper metadata.
- No default transcript-read tool in v1.
- V1 only compacts text-only replies. Mixed text+image replies remain unchanged so current text/image ordering is preserved.

## Rejected Options

- Reviving or depending on modal pager behavior for the default public surface.
- Worker-owned transcript files tied to the worker tempdir lifetime.
- Adding a default output-read MCP tool before validating the simpler “show the path in reply text” workflow.
- Performing head/tail compaction in reader threads or in the tape itself.
- Treating detached idle output like a timed-out request that must be explicitly drained before new input can run.
- Treating the current seal-time implementation strategy as the long-term architecture.

## Concrete Examples

### Timeout with hidden file but no disclosed path

1. `repl(input="cat('a\\n'); flush.console(); Sys.sleep(1); cat('b\\n')", timeout_ms=100)`
2. Visible reply:

```text
> cat('a\n'); flush.console(); Sys.sleep(1); cat('b\n')
a
<<console status: busy, write_stdin timeout reached; elapsed_ms=N>>
```

3. At this point:
   - reply-tracking state: timed-out request still owes worker text
   - file materialization state: hidden worker transcript file
4. `repl(input="", timeout_ms=500)`
5. Visible reply:

```text
b
> 
```

6. The request is finished.
7. No response was oversized, so the client never saw the file path.

What the hidden file contains after step 5:

```text
> cat('a\n'); flush.console(); Sys.sleep(1); cat('b\n')
a
b
> 
```

It does not contain the timeout marker.

### Four polls: small, small, spill, small

1. `repl(input="very noisy long command", timeout_ms=100)`
2. Timeout reply is small.
3. `repl(input="", timeout_ms=200)` returns a small poll reply.
4. `repl(input="", timeout_ms=200)` returns a second small poll reply.
5. `repl(input="", timeout_ms=1000)` is the first oversized poll reply.
6. `repl(input="", timeout_ms=200)` returns a final small poll reply.

Behavior:

- the hidden worker transcript file was already created at step 2
- step 5 is the first time the path is disclosed
- the step-5 inline reply shows only the new worker-originated text from step 5, compacted as needed
- after step 6, the same file contains the full worker-originated transcript from step 2 onward, including worker text that had already been shown inline in steps 2, 3, and 4

### Detached idle output does not block `repl(input=...)`

1. `repl(input="normal command", timeout_ms=1000)` finishes normally.
2. Later, a forked child inherited the pipe and starts writing idle output.
3. No request is active now.
4. `repl(input="1+1", timeout_ms=1000)`

Behavior:

- accept `1+1`
- do not require the user to drain idle output first
- prepend at most a bounded detached-idle preview/notice
- if detached idle text is too large, compact or quarantine only that detached-idle portion and show its file path
- then show the `1+1` reply normally

## Next Safe Slice

- Decide whether session-end notices should stay purely inline-only all the way through the existing tape path.
- Add focused public coverage for detached idle output that later becomes oversized.
- Revisit the seal-time implementation once the public contract is stable enough to justify a more eager consumer.
- Leave quota, retention policy, transcript-read tools, and the later eager/state-machine consumer out of the current slice.

## Stop Conditions

- Stop if file append still happens before the final worker-derived text for a reply is known.
- Stop if the implementation requires worker protocol changes or pushes formatting logic into reader threads.
- Stop if mixed text+image replies cannot stay unchanged without hidden reordering; keep them out of v1.
- Stop if detached-idle handling starts forcing polls before new input can run.
- Stop if the implementation starts hard-coding seal-time mechanics into the public contract. Seal-time is a phase tactic, not the product definition.
- Stop if the current phase starts accumulating complexity whose only purpose is to preserve the seal-time tactic. If the tactic gets in the way, record the issue and revisit the phased rollout.

## Decision Log

- 2026-03-21: Ignore the pager for the new design. Treat it as legacy behavior outside the default `files` mode.
- 2026-03-21: Prefer full omitted-output retrieval over permanent middle-drop, but do it through visible file paths rather than a default read tool.
- 2026-03-21: Default retrieval path is “show the file path in normal reply text.” A dedicated read tool, if ever needed, is future work and should be opt-in.
- 2026-03-21: Polling is the primary workflow. Agents are expected to keep polling with bounded replies and use filesystem tools only when the preview is not enough.
- 2026-03-21: The tape is always-on and always collecting, including output that arrives between requests.
- 2026-03-21: For the current implementation phase, oversized-output handling should happen only at seal time after draining the tape and rendering the final reply.
- 2026-03-21: Prefer line-first preview and metadata. Fall back to char mode only when clean line-aligned preview would not fit the budget.
- 2026-03-21: Keep the reply plain: one middle marker line with shown ranges and the file path, not multiple metadata lines.
- 2026-03-21: The formatter must live on the server side because the exact visible text is finalized there after image collapsing and prompt/error normalization.
- 2026-03-21: Worker transcript files must be server-owned and tied to server lifetime, not worker lifetime.
- 2026-03-21: Worker transcript files contain only worker-originated REPL text and exclude server-only notices.
- 2026-03-21: Worker transcript files include prompts and echoed input exactly as surfaced from the worker-side interaction.
- 2026-03-21: Timed-out requests create hidden files immediately to avoid unbounded in-memory accumulation.
- 2026-03-21: Hidden file paths are disclosed only when truncation or quarantine is actually surfaced in a response.
- 2026-03-21: Detached idle output remains non-blocking; it may be previewed or quarantined, but it does not make `repl(input=...)` unusable.
- 2026-03-21: Scope v1 to text-only oversized replies and leave mixed text+image replies unchanged.
- 2026-03-21: The current seal-time phase is a bounded implementation step chosen to keep iteration simple while the public behavior is still moving.
- 2026-03-21: The likely long-term direction is a more eager/state-machine-driven consumer of tape output with a better memory profile. Do not let the current seal-time phase rename or redefine the broader initiative.
