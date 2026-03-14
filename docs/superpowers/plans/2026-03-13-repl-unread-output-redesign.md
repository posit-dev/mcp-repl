# REPL Unread Output Redesign Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace ring-based unread tracking and transport-coupled overflow retention with server-owned unread batching, server-sealed reply batches, and per-reply overflow artifacts that match the redesign spec while keeping only the documented no-op compatibility acceptance for legacy overflow-consumed notifications.

**Architecture:** Move unread capture ownership into a server-owned `PendingOutput` store that survives idle periods and session lifecycle transitions until the next tool response drains it. Make `PendingOutput` the single sink for every output producer: stdout/stderr reader threads, image events, and synthetic server-side notices such as session-end, restart, guardrail, and capture-failure messages. The worker should stop assembling user-visible replies and instead return only low-level execution/completion metadata needed for the server to decide whether the request became idle, timed out, or ended the session. The server owns the public reply lifecycle: wait for idle or deadline, apply a short settle window after sideband-idle so final reader-thread writes can land, seal one drained batch, then format that batch into inline MCP content plus an optional retained per-reply artifact. Stop emitting overflow response tokens and remove transport-coupled retention, but keep `codex/overflow-response-consumed` accepted as a no-op compatibility notification during the transition.

**Tech Stack:** Rust, Tokio, rmcp, tempfile, serde, integration tests in `tests/*.rs`, snapshot tests via insta

---

## File Map

- Create: `src/pending_output.rs` — server-owned unread-output store, in-memory/spill states, destructive drain API, inline-policy support for privileged items, internal stale-reader guard, latched capture-failure state, and startup env parsing for unread-memory budget / unread spill root
- Create: `src/server/reply_batch.rs` — turn one server-sealed drained batch plus completion metadata into MCP content, head-and-tail preview builder, input echo and prompt cleanup integration, preserved echo-collapse behavior, and guaranteed inline handling for privileged items
- Create: `src/server/overflow_artifacts.rs` — retained per-reply artifact writer, eviction policy, and startup env parsing for overflow root / retention caps
- Modify: `src/main.rs` — register the new `pending_output` module and stop registering the deleted `output_capture` module
- Modify: `src/server.rs` — own `PendingOutput` and overflow artifacts, own the wait/settle/seal lifecycle for `repl` and `repl_reset`, and reduce the legacy overflow notification path to a no-op compatibility handler
- Modify: `src/debug_repl.rs` — keep the standalone debug REPL compiling if `WorkerManager::new(...)` or reply formatting ownership changes
- Modify: `src/worker_process.rs` — route every output producer into `PendingOutput`, stop building user-visible reply batches, and return only the execution/completion metadata the server needs to settle and seal one batch
- Modify: `src/worker_protocol.rs` — replace MCP-shaped worker replies with minimal server-consumed execution/completion outcomes and remove ring-truncation fields
- Delete: `src/output_capture.rs` — old ring-buffer capture and replay-gap truncation logic
- Delete: `src/server/response.rs` — move surviving logic into `reply_batch.rs` and `overflow_artifacts.rs`
- Modify: `tests/common/mod.rs` — drop overflow token / ack helpers, keep only any minimal raw-notification helper needed for no-op compatibility coverage, and add server env wiring for public overflow write-failure and pending-output spill-failure coverage
- Modify: `tests/write_stdin_batch.rs` — non-overlapping batch, background output, poll/wait semantics, huge echo preservation
- Modify: `tests/write_stdin_behavior.rs` — preserve public stderr/busy/error-prompt behavior during the capture refactor
- Modify: `tests/interrupt.rs` — combined `\u0003`/`\u0004` plus remaining-input semantics
- Modify: `tests/session_endings.rs` — session-end output and respawn output in one later reply
- Modify: `tests/manage_session_behavior.rs` — respawn/reset lifecycle semantics and unread co-batching
- Modify: `tests/repl_surface.rs` — `repl_reset` lifecycle semantics on the same unread-drain path
- Modify: `tests/write_stdin_edge_cases.rs` — zero-timeout and empty-input semantics under destructive unread drains
- Modify: `tests/plot_images.rs` — oversized reply artifact behavior, head-and-tail previews, always-inline privileged notices, self-contained artifact and cleanup-retry retention coverage, and retirement of token-aware transport-retention coverage
- Modify: `tests/python_plot_images.rs` — long-line preview slicing and image-path preview rules
- Modify: `tests/python_backend.rs` — replace the old truncation-contract assertion with the new spill-backed unread contract
- Modify: `tests/r_file_show.rs` — visible overflow wording if file-overflow assertions need updating
- Modify: `docs/tool-descriptions/repl_tool_r.md`
- Modify: `docs/tool-descriptions/repl_tool_python.md`
- Modify: `docs/tool-descriptions/repl_reset_tool.md`

## Chunk 1: Server-Owned Unread Output And Request Semantics

### Task 1: Lock in the public unread-output semantics with failing tests

**Files:**
- Modify: `tests/write_stdin_batch.rs`
- Modify: `tests/write_stdin_behavior.rs`
- Modify: `tests/interrupt.rs`
- Modify: `tests/session_endings.rs`
- Modify: `tests/repl_surface.rs`
- Modify: `tests/write_stdin_edge_cases.rs`
- Modify: `tests/common/mod.rs`

- [ ] **Step 1: Write failing public tests for the new batch semantics**

Add tests that drive only the public `repl`/`write_stdin` surface:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_background_output_while_idle_prefixes_next_reply() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let _ = session
        .write_stdin_raw_with(
            "system2(command = file.path(R.home(\"bin\"), \"Rscript\"), args = c(\"-e\", \"Sys.sleep(0.2); cat('BG\\\\n')\"), wait = FALSE, stdout = \"\", stderr = \"\")",
            Some(10.0),
        )
        .await?;

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let result = session.write_stdin_raw_with("cat('NEXT\\n')", Some(10.0)).await?;
    let text = collect_text(&result);
    assert!(text.contains("BG"));
    assert!(text.contains("NEXT"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_then_run_returns_one_combined_batch() -> TestResult<()> {
    let mut session = common::spawn_server().await?;
    let _ = session
        .write_stdin_raw_with("cat('start\\n'); flush.console(); Sys.sleep(5)", Some(0.2))
        .await?;

    let result = session
        .write_stdin_raw_with("\u{3}cat('after\\n')", Some(10.0))
        .await?;
    let text = collect_text(&result);
    assert!(text.contains("start"));
    assert!(text.contains("after"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn session_end_notice_and_respawn_output_can_share_one_reply() -> TestResult<()> {
    let mut session = common::spawn_server().await?;
    let _ = session.write_stdin_raw_with("quit(\"no\")", Some(10.0)).await?;
    let result = session.write_stdin_raw_with("1+1", Some(10.0)).await?;
    let text = collect_text(&result);
    assert!(text.contains("session ended") || text.contains("new session started"));
    assert!(text.contains("2"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_reset_includes_unread_output_in_its_reply() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let _ = session
        .write_stdin_raw_with(
            "system2(command = file.path(R.home(\"bin\"), \"Rscript\"), args = c(\"-e\", \"Sys.sleep(0.2); cat('OLD\\\\n')\"), wait = FALSE, stdout = \"\", stderr = \"\")",
            Some(10.0),
        )
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let result = session.call_tool_raw("repl_reset", serde_json::json!({})).await?;
    let text = collect_text(&result);
    assert!(text.contains("OLD"));
    assert!(text.contains("new session started"));
    Ok(())
}
```

Use `Rscript` rather than `/bin/sh` so the test stays portable across supported platforms. If the background-output assertion is flaky on slower CI, wrap the idle wait in a short deadline-based retry loop rather than relying on one narrow fixed sleep.

Also add one public spill-failure regression using only the MCP surface. Start the server with `MCP_CONSOLE_PENDING_OUTPUT_MEMORY_BYTES` set tiny and `MCP_CONSOLE_PENDING_OUTPUT_SPILL_ROOT` pointed at an unwritable directory, then assert:
- the first plain `repl(...)` call after the spill failure returns a deterministic capture-failure batch and does not run the requested code
- a second plain `repl(...)` call and an idle `repl("", ...)` poll both return the same deterministic capture-failure batch again
- `repl_reset` still succeeds and returns `[repl] new session started`
- a later plain `repl(...)` call runs normally again after reset

Also update the existing timeout-polling regression in `tests/write_stdin_batch.rs` so it locks in the new drain semantics rather than the old busy-discard behavior:
- after a timed-out request, the first `repl("", timeout=T)` waits until the worker reaches idle and returns the unread tail exactly once
- an immediate second `repl("", timeout=T)` returns the idle marker or only newly arrived output, never the same unread tail again

Also add one public regression that locks in the server settle window. Drive a request that reaches sideband-idle while one final stdout/stderr chunk is still being drained by the reader threads, then assert:
- the first reply already contains that final chunk
- the next poll does not repeat it
- the assertion is phrased around one logical batch, not around an implementation-specific sleep

Also update `tests/write_stdin_edge_cases.rs` so the edge contracts move with the redesign instead of being checked only at the end:
- `timeout=0` still behaves like an already-expired deadline, and any unread tail from that request is drainable on the next poll exactly once
- empty input with no unread output still returns the idle marker / prompt path, while empty input with unread output drains unread output instead of returning only the idle marker

Also add public control-byte deadline regressions for the failure branch the spec calls out:
- `\u0003 + code` skips the trailing code payload when the interrupt phase does not reach idle before the deadline
- `\u0004 + code` skips the trailing code payload when the restart phase does not reach idle before the deadline

Do not delete the existing public assertions in `tests/write_stdin_behavior.rs` for `stderr:` framing, busy-discard wording, and error-prompt normalization; keep them green throughout the refactor so the capture rewrite cannot silently change those contracts.

- [ ] **Step 2: Run the targeted tests to verify they fail**

Run:

```bash
cargo test --test write_stdin_batch write_stdin_background_output_while_idle_prefixes_next_reply -- --exact
cargo test --test write_stdin_batch write_stdin_timeout_polling_waits_until_idle_and_drains_once -- --exact
cargo test --test repl_surface capture_failure_blocks_plain_input_until_reset -- --exact
cargo test --test repl_surface capture_failure_allows_ctrl_d_recovery -- --exact
cargo test --test interrupt interrupt_then_run_returns_one_combined_batch -- --exact
cargo test --test interrupt interrupt_prefix_timeout_skips_remaining_input -- --exact
cargo test --test interrupt restart_prefix_timeout_skips_remaining_input -- --exact
cargo test --test session_endings session_end_notice_and_respawn_output_can_share_one_reply -- --exact
cargo test --test repl_surface repl_reset_includes_unread_output_in_its_reply -- --exact
cargo test --test write_stdin_edge_cases write_stdin_timeout_zero_is_non_blocking -- --exact
cargo test --test write_stdin_edge_cases write_stdin_empty_returns_prompt -- --exact
cargo test --test write_stdin_batch write_stdin_settles_final_output_before_draining -- --exact
```

Expected: FAIL because the current ring/poll logic still discards or splits these batches according to the old semantics. In particular, idle polls do not yet wait-until-idle and drain once, lifecycle requests do not yet consistently return all output collected since the previous tool response, and the control-byte prefix path does not yet pin the timeout branch that must skip the trailing payload.

- [ ] **Step 3: Commit the red tests**

```bash
git add tests/write_stdin_batch.rs tests/write_stdin_behavior.rs tests/interrupt.rs tests/session_endings.rs tests/repl_surface.rs tests/write_stdin_edge_cases.rs tests/common/mod.rs
git commit -m "test: lock in unread-output redesign semantics"
```

### Task 2: Introduce the server-owned unread buffer and cut the live reply path over to it

**Files:**
- Create: `src/pending_output.rs`
- Modify: `src/main.rs`
- Modify: `src/server.rs`
- Modify: `src/server/response.rs`
- Modify: `src/debug_repl.rs`
- Modify: `src/worker_process.rs`
- Modify: `src/worker_protocol.rs`

- [ ] **Step 1: Create the new unread-output store**

Add `src/pending_output.rs` with one clear owner-facing API:

```rust
pub(crate) enum InlinePolicy {
    Normal,
    MustInline,
}

pub(crate) enum PendingItem {
    Text { text: String, stream: TextStream, inline_policy: InlinePolicy },
    Image { id: String, mime_type: String, data: String, is_new: bool, inline_policy: InlinePolicy },
}

pub(crate) struct DrainedBatch {
    pub items: Vec<PendingItem>,
}

pub(crate) struct PendingOutput { /* in-memory or spilled */ }

impl PendingOutput {
    pub(crate) fn new() -> Result<Self, PendingOutputError>;
    pub(crate) fn append_text(&self, text: String, stream: TextStream, inline_policy: InlinePolicy) -> Result<(), PendingOutputError>;
    pub(crate) fn append_image(&self, id: String, mime_type: String, data: String, is_new: bool, inline_policy: InlinePolicy) -> Result<(), PendingOutputError>;
    pub(crate) fn has_unread(&self) -> bool;
    pub(crate) fn drain(&self) -> DrainedBatch;
    pub(crate) fn mark_capture_failed(&self, message: String);
    pub(crate) fn capture_failure_message(&self) -> Option<String>;
    pub(crate) fn reset_for_restart(&self);
}
```

Keep this module focused on unread storage only:
- no prompt logic
- no overflow file retention
- no transport lifecycle logic

Semantics to lock in here:
- `PendingOutput::new()` owns the startup env parsing for unread storage, including `MCP_CONSOLE_PENDING_OUTPUT_MEMORY_BYTES` and `MCP_CONSOLE_PENDING_OUTPUT_SPILL_ROOT`; invalid values fail fast at startup
- unread storage is server-owned and may spill instead of truncating
- every output write path, including synthetic server notices, goes through `PendingOutput`
- writers can mark items as `MustInline` so they are guaranteed to appear in the tool response rather than only in an overflow artifact
- lifecycle requests do not clear unread output; the next response drains everything collected since the previous tool response
- spill-promotion failure latches a capture-failure state that survives `drain()` and every plain `repl(...)` / `repl("", ...)` call until explicit restart/reset clears it
- if an internal stale-reader guard is needed to avoid double-appends after teardown, keep it private to the implementation and do not make process identity part of the public drain semantics

- [ ] **Step 2: Register the new module in the crate**

Add `mod pending_output;` in `src/main.rs` and import the new types at the first call sites that will own or consume them.

- [ ] **Step 3: Run the partially wired tree through a failing build**

Run:

```bash
cargo check
```

Expected: FAIL with unresolved imports or constructor/call-site mismatches until the server, debug REPL, and worker wiring is updated.

- [ ] **Step 4: Make the server own the buffer**

Modify `src/server.rs` so `SharedServer` owns `Arc<PendingOutput>` and passes it into `WorkerManager::new(...)`.

- [ ] **Step 5: Update the other direct `WorkerManager` constructor callers**

If `WorkerManager::new(...)` takes `PendingOutput` (or a factory for it), update the direct non-server callers too:
- `src/debug_repl.rs`
- unit tests in `src/worker_process.rs`

Do not leave the debug REPL on a stale constructor shape while the server compiles.

- [ ] **Step 6: Port every append path into `PendingOutput` and slim the worker outcome API**

Modify `src/worker_process.rs` so the existing stdout/stderr/image reader path appends directly into `PendingOutput` instead of `OutputTimeline`/`OutputBuffer`. Preserve the existing always-on reader-thread behavior while the worker is idle. Then replace the direct synthetic append sites too: guardrail notices, session-end notices, restart notices, and future capture-failure notices must all use the same `PendingOutput` API with `InlinePolicy::MustInline` where the message must remain visible in the tool response.

At the same time, stop building user-visible `WorkerReply::Output` payloads in the worker. Replace that with a minimal worker-to-server outcome carrying only what the server needs to seal a batch correctly, for example:
- whether the operation completed, timed out, was rejected as busy, or ended the session
- prompt / prompt-variant metadata needed for prompt stripping and prompt re-append
- echo events needed for echo-only elision or collapse
- any machine-readable error code the server still needs for public wording

If you need an internal stale-reader guard to prevent duplicate appends after teardown, keep it as an implementation detail and do not let it drop output that should still be surfaced by the next tool response.

- [ ] **Step 6a: Keep the debug REPL functional through the protocol cut-over**

Because Task 2 changes the worker outcome shape, update `src/debug_repl.rs` in this chunk too. Either:
- add a temporary adapter from the new minimal worker outcome into the current terminal rendering flow, or
- route the debug REPL through the same server-side formatting helper that now owns prompt/error/output shaping

Do not defer this to later cleanup work; the standalone debug REPL should still compile and remain usable while Task 2 is in progress.

- [ ] **Step 7: Move wait/settle/seal ownership into `src/server.rs` before re-running tests**

Modify `src/server.rs` so the request lifecycle becomes:
1. ask the worker to execute / interrupt / restart / poll and wait for the low-level outcome
2. if the worker reported idle before the deadline, run a short settle window so any in-flight reader-thread writes can land
3. drain `PendingOutput` exactly once to seal the batch
4. pass the drained batch plus completion metadata into a server-side formatting path

Semantics to preserve while moving ownership:
- `repl`, `repl("", ...)`, `\u0003`, `\u0004`, and compound control-byte forms still use one overall deadline
- background prefix output is just unread content already sitting in `PendingOutput`
- busy / timeout / lifecycle wording remains public-server behavior, not worker-side response assembly
- if any ring-based helper must survive briefly during the refactor, limit it to private capture/echo support only; the returned public batch must be sealed by the server before this task is considered done
- by the end of this step, the unread semantics tests from Task 1 should be exercising the real new path, not a mixed sink/source transitional state

Do not introduce a user-facing settle-window knob unless the implementation proves one is necessary. A small internal constant is enough.
It is fine to adapt the existing `src/server/response.rs` flow temporarily for this cut-over; Task 5 performs the clean extraction into `reply_batch.rs` and `overflow_artifacts.rs`.

- [ ] **Step 8: Preserve the public echo-collapse behavior in the server formatter before deleting the ring helpers**

Before removing `OutputBuffer`-based snapshots, move or rewrite the current `IpcEchoEvent`-driven behaviors so they still run over one server-sealed drained batch:
- drop pure echo-only output for large silent inputs
- collapse large echoed transcripts while preserving attribution markers
- keep prompt trimming behavior for single-expression inputs

Do not regress the existing public tests in `tests/write_stdin_batch.rs` that cover large echo handling.

- [ ] **Step 9: Re-run the focused tests**

Run:

```bash
cargo test --test write_stdin_batch write_stdin_background_output_while_idle_prefixes_next_reply -- --exact
cargo test --test write_stdin_batch write_stdin_timeout_polling_waits_until_idle_and_drains_once -- --exact
cargo test --test session_endings session_end_notice_and_respawn_output_can_share_one_reply -- --exact
cargo test --test repl_surface repl_reset_includes_unread_output_in_its_reply -- --exact
cargo test --test write_stdin_edge_cases write_stdin_timeout_zero_is_non_blocking -- --exact
cargo test --test write_stdin_edge_cases write_stdin_empty_returns_prompt -- --exact
cargo test --test write_stdin_behavior write_stdin_mixed_stdout_stderr -- --exact
cargo test --test write_stdin_behavior write_stdin_discards_when_busy -- --exact
cargo test --test repl_surface capture_failure_allows_ctrl_d_recovery -- --exact
cargo test --test write_stdin_batch write_stdin_settles_final_output_before_draining -- --exact
cargo test --test write_stdin_batch write_stdin_drops_huge_echo_only_inputs -- --exact
cargo test --test write_stdin_batch write_stdin_collapses_huge_echo_with_output_attribution -- --exact
```

Expected: PASS for the unread-drain semantics and echo-focused regressions. Oversized-preview and artifact-retention tests may still fail later because reply formatting still uses the old overflow presentation.

- [ ] **Step 10: Commit the cut-over work**

```bash
git add src/pending_output.rs src/main.rs src/server.rs src/server/response.rs src/debug_repl.rs src/worker_process.rs src/worker_protocol.rs
git commit -m "refactor: cut repl output over to server-owned pending output"
```

### Task 3: Remove the remaining ring-only machinery and finalize the new public contract

**Files:**
- Modify: `src/worker_process.rs`
- Modify: `src/worker_protocol.rs`
- Modify: `src/debug_repl.rs`
- Delete: `src/output_capture.rs`
- Modify: `src/main.rs`
- Modify: `src/server.rs`
- Modify: `tests/common/mod.rs`
- Modify: `tests/write_stdin_batch.rs`
- Modify: `tests/interrupt.rs`
- Modify: `tests/session_endings.rs`
- Modify: `tests/manage_session_behavior.rs`
- Modify: `tests/repl_surface.rs`
- Modify: `tests/write_stdin_edge_cases.rs`
- Modify: `tests/python_backend.rs`

- [ ] **Step 1: Remove transitional reply-shape compatibility and lock the protocol boundary**

Now that Task 2 has introduced the minimal worker outcome and updated its call sites, use this task to remove any transitional compatibility shims:
- delete any leftover `WorkerReply::Output` variants or adapters kept only to bridge the cut-over
- remove `older_output_dropped` and any other fields that only existed because the worker used to prepare user-visible replies
- simplify `src/debug_repl.rs` so it consumes the stabilized post-cut-over interface rather than a temporary bridge
- keep the smallest server-consumed outcome shape that still supports busy rejection, timeout vs idle completion, prompt cleanup metadata, echo-collapse metadata, and session-end / restart distinctions

Update serde, constructors, debug REPL plumbing, and tests accordingly.

- [ ] **Step 2: Remove the remaining ring-only helpers from the worker path**

In `src/worker_process.rs`:
- delete the old offset/snapshot helpers that were only needed to read from `OutputBuffer`
- remove any now-dead branching that distinguished pending-prefix snapshots from live unread drains
- keep only the logic that is still required for the new product semantics on the worker side: process execution, busy state, sideband completion metadata, and capture-failure latching

In `src/server.rs` / `src/server/reply_batch.rs`:
- keep the public reply semantics that moved out of the worker: settle-window sealing, prompt cleanup, busy/timeout/lifecycle wording, and echo shaping over drained batches

- [ ] **Step 3: Update the public truncation and capture-failure contracts**

Because unread output is now server-owned and may spill instead of truncating, remove the public contract that says older unread output disables full-response artifacts. Replace it with a public assertion that oversized unread batches still produce a bounded inline preview plus a retained artifact when the server can spill/write successfully. At the same time, add the public assertion that spill-promotion failure latches a capture-failure state until explicit reset/restart.

- [ ] **Step 4: Delete the dead ring machinery**

Remove `src/output_capture.rs`, its imports, its tests, and any ring-offset bookkeeping in `src/worker_process.rs`, `src/server.rs`, and `tests/common/mod.rs`.
Also remove `mod output_capture;` from `src/main.rs`.

- [ ] **Step 5: Run the semantics tests**

Run:

```bash
cargo test --test write_stdin_batch write_stdin_background_output_while_idle_prefixes_next_reply -- --exact
cargo test --test write_stdin_batch write_stdin_timeout_polling_waits_until_idle_and_drains_once -- --exact
cargo test --test interrupt interrupt_then_run_returns_one_combined_batch -- --exact
cargo test --test interrupt interrupt_prefix_timeout_skips_remaining_input -- --exact
cargo test --test interrupt restart_prefix_timeout_skips_remaining_input -- --exact
cargo test --test session_endings session_end_notice_and_respawn_output_can_share_one_reply -- --exact
cargo test --test manage_session_behavior restart_while_busy_resets_session -- --exact
cargo test --test repl_surface repl_reset_clears_state -- --exact
cargo test --test repl_surface repl_reset_includes_unread_output_in_its_reply -- --exact
cargo test --test repl_surface capture_failure_blocks_plain_input_until_reset -- --exact
cargo test --test repl_surface capture_failure_allows_ctrl_d_recovery -- --exact
cargo test --test write_stdin_edge_cases write_stdin_timeout_zero_is_non_blocking -- --exact
cargo test --test write_stdin_behavior write_stdin_mixed_stdout_stderr -- --exact
cargo test --test python_backend python_truncated_pending_prefix_spills_to_server_owned_artifact -- --exact
cargo test --test write_stdin_batch write_stdin_settles_final_output_before_draining -- --exact
```

Expected: PASS for the new semantics, including lifecycle consistency. Snapshots may still fail later because the formatter still uses the old overflow presentation.

- [ ] **Step 6: Commit the semantic cut-over**

```bash
git add -A src tests/common/mod.rs tests/write_stdin_batch.rs tests/interrupt.rs tests/session_endings.rs tests/manage_session_behavior.rs tests/repl_surface.rs tests/write_stdin_edge_cases.rs tests/python_backend.rs
git commit -m "cleanup: remove ring-only repl output helpers"
```

## Chunk 2: Reply Formatting, Overflow Artifacts, And Cleanup

### Task 4: Lock in oversized-preview behavior with failing public tests

**Files:**
- Modify: `tests/plot_images.rs`
- Modify: `tests/python_plot_images.rs`
- Modify: `tests/common/mod.rs`

- [ ] **Step 1: Add failing public tests for oversized replies**

Add tests for the spec rules that changed:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn oversized_reply_preview_shows_non_overlapping_head_and_tail() -> TestResult<()> {
    let mut session = common::spawn_server().await?;
    let input = "\
cat('HEAD-0000\\n'); \
for (i in 1:5000) cat(sprintf('MID-%04d\\n', i)); \
cat('TAIL-9999\\n')";
    let result = session.write_stdin_raw_with(input, Some(10.0)).await?;
    let text = collect_text(&result);
    assert!(text.contains("HEAD-0000"));
    assert!(text.contains("TAIL-9999"));
    assert!(text.contains("[repl] middle of this reply omitted from inline preview"));
    assert!(!text.contains("MID-2500"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_single_line_respects_total_budget() -> TestResult<()> {
    let mut session = common::spawn_python_server().await?;
    let result = session.write_stdin_raw_with("print('x' * 200000)", Some(10.0)).await?;
    let text = collect_text(&result);
    assert!(text.len() < 20_000);
    assert!(text.contains("[repl] middle of this reply omitted from inline preview"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn preview_never_shows_partial_full_image_path_notice() -> TestResult<()> {
    let mut session = common::spawn_python_server().await?;
    let input = fake_plot_image_script(240, 10_000, 12_000);
    let result = session.write_stdin_raw_with(&input, Some(120.0)).await?;
    let text = collect_text(&result);
    let notice_lines = text
        .lines()
        .filter(|line| line.contains("full image at "))
        .count();
    let paths = extract_all_paths(&text, "full image at ");
    assert_eq!(
        paths.len(),
        notice_lines,
        "expected every visible image notice to contain one complete parseable path: {text:?}"
    );
    for path in paths {
        assert!(path.exists(), "expected complete visible path: {path:?}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn overflow_write_failure_returns_bounded_preview_with_notice() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let read_only = temp.path().join("overflow-root");
    std::fs::create_dir(&read_only)?;
    let mut perms = std::fs::metadata(&read_only)?.permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&read_only, perms)?;

    let mut session = common::spawn_server_with_env_vars(vec![
        (
            "MCP_CONSOLE_OVERFLOW_ROOT".to_string(),
            read_only.display().to_string(),
        ),
    ])
    .await?;
    let result = session
        .write_stdin_raw_with("cat(paste(rep('line', 5000), collapse='\\n'))", Some(10.0))
        .await?;
    let text = collect_text(&result);
    assert!(text.contains("could not write full reply artifact"));
    assert!(text.len() < 20_000);
    Ok(())
}
```

Reuse the existing `fake_plot_image_script(...)` helper in `tests/python_plot_images.rs` for the path-notice case; do not introduce a separate `test_plot_fixture` module just for this test. If the overflow write-failure case cannot be driven with the current harness, add a server startup env var such as `MCP_CONSOLE_OVERFLOW_ROOT` and use `spawn_server_with_env_vars(...)` to point the server at a read-only directory. Do not add a test-only constructor to the product code.

Also add one public regression that forces a reset reply to overflow and asserts `[repl] new session started` remains inline in the returned tool response even when most of the batch is moved into the retained artifact. This locks in the privileged `MustInline` path.

Also add retention regressions that lock in the new per-reply artifact model rather than the old transport-lifetime model:
- two oversized replies create two different artifact paths with non-overlapping contents
- with a tiny reply-count cap configured at startup, an older artifact path disappears after enough newer oversized replies are finalized

Keep the existing self-contained-artifact contract while rewriting retention. Add or preserve a public regression that:
- reads the retained text artifact for an oversized reply
- extracts every `full image at ...` path referenced by that text artifact
- asserts those image paths still exist for as long as that text artifact path still exists

Also keep the current cleanup-retry contract. Add or preserve a public regression that:
- forces one eviction/delete failure
- triggers later oversized replies after the failure is removed
- asserts retention comes back under the configured cap instead of remaining permanently over-cap

Also add public startup-config regressions for the new overflow env parsing:
- invalid `MCP_CONSOLE_OVERFLOW_MAX_REPLIES` / `MCP_CONSOLE_OVERFLOW_BYTE_CAP` values fail fast at server startup instead of silently disabling overflow retention
- a tiny `MCP_CONSOLE_OVERFLOW_BYTE_CAP` evicts older retained artifacts even when the reply-count cap alone would keep them

Drive the retention regression with real startup env vars, not test-only constructors. Use `MCP_CONSOLE_OVERFLOW_MAX_REPLIES` for the tiny reply-count cap and `MCP_CONSOLE_OVERFLOW_BYTE_CAP` if you also need to force byte-cap eviction.

Do not keep any transport-retention coverage or helpers for `overflowResponseToken`; this redesign intentionally retires token-based retention. Keep at most one minimal compatibility regression that `codex/overflow-response-consumed` is accepted as a no-op even though the server no longer emits `overflowResponseToken`.

- [ ] **Step 2: Run the targeted overflow tests to verify they fail**

Run:

```bash
cargo test --test plot_images oversized_reply_preview_shows_non_overlapping_head_and_tail -- --exact
cargo test --test plot_images repl_reset_overflow_keeps_restart_notice_inline -- --exact
cargo test --test plot_images multiple_oversized_replies_create_distinct_artifacts_without_overlap -- --exact
cargo test --test plot_images overflow_artifact_eviction_removes_older_paths -- --exact
cargo test --test plot_images overflow_artifact_byte_cap_evicts_older_paths -- --exact
cargo test --test plot_images persisted_overflow_reply_keeps_image_artifacts_live -- --exact
cargo test --test plot_images overflow_cleanup_retries_after_delete_failure -- --exact
cargo test --test plot_images overflow_consumed_notification_is_accepted_as_noop -- --exact
cargo test --test plot_images invalid_overflow_env_fails_fast_at_startup -- --exact
cargo test --test python_plot_images oversized_single_line_respects_total_budget -- --exact
cargo test --test python_plot_images preview_never_shows_partial_full_image_path_notice -- --exact
cargo test --test plot_images overflow_write_failure_returns_bounded_preview_with_notice -- --exact
```

Expected: FAIL for the new preview, retention, and eviction assertions because the current formatter is head-only, line-biased, and still tied to the old overflow store.

- [ ] **Step 3: Commit the red overflow tests**

```bash
git add tests/plot_images.rs tests/python_plot_images.rs tests/common/mod.rs
git commit -m "test: lock in oversized reply preview behavior"
```

### Task 5: Replace `response.rs` with per-reply formatting and artifact retention

**Files:**
- Create: `src/server/reply_batch.rs`
- Create: `src/server/overflow_artifacts.rs`
- Modify: `src/server.rs`
- Delete: `src/server/response.rs`

- [ ] **Step 1: Move retained-artifact ownership into its own module**

Create `src/server/overflow_artifacts.rs` with a store keyed by completed reply batches, not by transport-delivery state. It should:
- own startup env parsing for `MCP_CONSOLE_OVERFLOW_ROOT`, `MCP_CONSOLE_OVERFLOW_MAX_REPLIES`, and `MCP_CONSOLE_OVERFLOW_BYTE_CAP` so integration tests can force write failures and eviction without test-only constructors; invalid values fail fast at startup
- write one self-contained text artifact plus any image files for one reply
- retain the most recent `N` replies plus a coarse byte cap
- evict oldest retained replies after a new reply is finalized
- retry cleanup on later retentions/requests after transient delete failures instead of treating one failed unlink as permanent
- stop emitting `overflowResponseToken` and delete any remaining transport-delivery bookkeeping rather than preserving it behind a compatibility shim

- [ ] **Step 2: Build the new reply formatter**

Create `src/server/reply_batch.rs` with one entry point that accepts:

```rust
pub(crate) fn format_drained_batch(
    batch: DrainedBatch,
    completion: CompletionMetadata,
    artifacts: Option<&OverflowArtifacts>,
    metadata: ReplyMetadata,
) -> CallToolResult
```

The formatter must:
- preserve image ordering after `collapse_image_updates`
- apply the server-owned settle/seal semantics before formatting: it must operate only on a batch that was drained after the idle-then-settle path finished
- apply the total inline-size budget, not a line-count quota
- produce a non-overlapping head slice, one synthetic middle-omission notice, and a tail slice
- prefer whole text lines when possible, but cut inside a single oversized text line if necessary
- never show a partial `full image at ...` notice inline
- never move `InlinePolicy::MustInline` items into overflow-only content; reserve inline budget for them and keep them visible in the tool response
- preserve the existing input-echo cleanup guarantees after the worker-side drain refactor, using only the drained batch plus completion metadata rather than worker-built reply content
- write a self-contained full-reply artifact when possible
- on artifact write failure, keep the bounded preview, add a short write-failure notice, and drop the undisplayed tail

- [ ] **Step 3: Replace the old server integration**

Modify `src/server.rs` to call the new formatter and artifact store. Delete the transport-completion hooks, pending-send maps, and response-token metadata emission from the old flow. Keep `codex/overflow-response-consumed` accepted as a no-op compatibility notification, but remove any remaining retention or lifetime semantics from it.

- [ ] **Step 4: Run the targeted overflow tests**

Run:

```bash
cargo test --test plot_images oversized_reply_preview_shows_non_overlapping_head_and_tail -- --exact
cargo test --test plot_images repl_reset_overflow_keeps_restart_notice_inline -- --exact
cargo test --test plot_images multiple_oversized_replies_create_distinct_artifacts_without_overlap -- --exact
cargo test --test plot_images overflow_artifact_eviction_removes_older_paths -- --exact
cargo test --test plot_images overflow_artifact_byte_cap_evicts_older_paths -- --exact
cargo test --test plot_images persisted_overflow_reply_keeps_image_artifacts_live -- --exact
cargo test --test plot_images overflow_cleanup_retries_after_delete_failure -- --exact
cargo test --test plot_images overflow_consumed_notification_is_accepted_as_noop -- --exact
cargo test --test plot_images invalid_overflow_env_fails_fast_at_startup -- --exact
cargo test --test python_plot_images oversized_single_line_respects_total_budget -- --exact
cargo test --test python_plot_images preview_never_shows_partial_full_image_path_notice -- --exact
cargo test --test plot_images overflow_write_failure_returns_bounded_preview_with_notice -- --exact
```

Expected: PASS.

- [ ] **Step 5: Commit the formatter/artifact rewrite**

```bash
git add -A src/server tests/plot_images.rs tests/python_plot_images.rs tests/common/mod.rs
git commit -m "refactor: rewrite repl overflow formatting per reply batch"
```

### Task 6: Remove dead coverage, refresh snapshots, and run the full verification suite

**Files:**
- Modify: `tests/common/mod.rs`
- Modify: `tests/write_stdin_batch.rs`
- Modify: `tests/session_endings.rs`
- Modify: `tests/interrupt.rs`
- Modify: `tests/manage_session_behavior.rs`
- Modify: `tests/repl_surface.rs`
- Modify: `tests/python_backend.rs`
- Modify: `tests/plot_images.rs`
- Modify: `tests/python_plot_images.rs`
- Modify: `tests/r_file_show.rs`
- Modify: `docs/tool-descriptions/repl_tool_r.md`
- Modify: `docs/tool-descriptions/repl_tool_python.md`
- Modify: `docs/tool-descriptions/repl_reset_tool.md`

- [ ] **Step 1: Delete dead helper coverage and retire only the intentionally removed transport-lifetime assertions**

Remove the `src/server/response.rs` unit tests and any `src/output_capture.rs` tests that are no longer reachable through the public MCP surface. Then remove the public raw-transport/acknowledgement assertions that only existed to cover transport-coupled overflow retention:
- `overflowResponseToken` helpers and ack plumbing in `tests/common/mod.rs`
- `send_overflow_consumed_ack(...)`, any token-aware custom notification helpers, and the raw-transport lifetime tests in `tests/plot_images.rs`
- any remaining assertions that overflow files stay alive until reply delivery rather than until per-reply retention eviction

Keep coverage in the integration tests for the new public contract:
- when a reply advertises an artifact path, that artifact exists at reply time
- retained text artifacts do not outlive the image artifacts they reference
- transient eviction failures are retried later and the store returns under cap
- distinct oversized replies do not share one artifact
- evicted older paths disappear according to the configured per-reply retention policy
- `codex/overflow-response-consumed` remains accepted as a no-op during compatibility if the spec still requires it

- [ ] **Step 2: Refresh snapshots and public assertions**

Before accepting snapshots:
- rename/update the old truncation-contract test in `tests/python_backend.rs`
- keep or add public assertions for huge echoed input handling, lifecycle-request consistency, non-overlapping idle-poll drains, retained-artifact behavior, privileged inline notices, and latched capture-failure recovery
- update `tests/r_file_show.rs` so it asserts the new preview / full-reply wording instead of the old `output truncated` contract
- verify the docs describe server-owned unread batching rather than worker-side truncation/acknowledgement
- update `docs/tool-descriptions/repl_reset_tool.md` so it no longer promises a reset reply that contains only the new-session status line if unread output is now co-batched into that response

Run:

```bash
cargo insta test
cargo insta pending-snapshots
```

Expected: pending snapshots in the REPL transcript suites that changed because of the new head-and-tail preview and drain semantics.

- [ ] **Step 3: Accept the intentional snapshot changes**

Run:

```bash
cargo insta accept
```

- [ ] **Step 4: Run the full required verification suite**

Run:

```bash
cargo +nightly fmt
cargo check
cargo build
cargo clippy
cargo test
```

Expected: all commands succeed cleanly.

- [ ] **Step 5: Commit the cleanup and verification pass**

```bash
git add docs/tool-descriptions/repl_tool_r.md docs/tool-descriptions/repl_tool_python.md docs/tool-descriptions/repl_reset_tool.md tests src
git commit -m "cleanup: remove ring-based repl output machinery"
```
