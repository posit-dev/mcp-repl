# Windows Runtime-Native Interrupt Wake

## Summary

- Deliver built-in Windows worker interrupts as real `CTRL_C_EVENT`
  notifications by writing exact ETX (`0x03`) to each worker's dedicated
  ConPTY input.
- Preserve R, Python, reticulate, and other package-installed console-handler
  policy. mcp-repl observes completion and wakes managed input, but never sets
  R or Python interrupt state.
- Keep sideband `interrupt` cleanup-only. It may discard queued, unconsumed
  managed input; it is not a second runtime interrupt authority.
- Add narrow, payload-free `interrupt_armed` and `interrupt_complete` ordering
  facts required by the public rearm and blocked-handler/tail-input
  regressions. Do not carry forward PR #122's nullable-prompt, broad
  readiness, input-ID, or general discard-ack redesign.
- Keep one worker process and the existing sideband reader. No polling timer,
  fixed handler delay, fallback delivery chain, or third process is introduced.

## Status

- State: completed
- Last updated: 2026-07-25
- Current phase: complete
- Branch: `fix/windows-native-interrupt-observer`
- Base: `origin/main` at `f072158c6f86398d663d3d6abfdb0176842daad0`

## Motivating Scenario

Accepted input reaches the worker-owned queue over sideband IPC. R's embedded
`ReadConsole` callback and Python's managed `input()` path therefore wait on
mcp-repl synchronization primitives, not directly on `ReadConsoleW`.

Windows invokes native console handlers on a separate Windows-created thread.
The runtime or a package handler decides what Ctrl-C means, and the runtime main
thread must later wake and execute its normal checkpoint:

- R: `R_CheckUserInterrupt()`
- Python: reacquire the GIL and call `PyErr_CheckSignals()`

reticulate is the motivating package. Its Windows handler bridges SIGINT state
between embedded R and Python and returns `TRUE`, consuming the native event
after establishing package-specific state. Whichever runtime observes the
interrupt first may clear counterpart state. mcp-repl must allow that policy to
run normally; it must not set `UserBreak`, `R_interrupts_pending`,
`PyErr_SetInterrupt*()`, or an mcp-repl-specific runtime-interrupt flag.

The public contract includes all three outcomes:

1. A normal runtime handler schedules an interrupt; the runtime checkpoint
   processes it after handler completion.
2. A consuming package handler schedules no runtime interrupt; managed input
   wakes, finds no pending interrupt, and continues waiting for later input.
3. A consuming package handler establishes runtime-specific state; the runtime
   main thread observes that state only after the handler has finished.

## Repository And PR History

The branch was created fresh from the fetched `origin/main`. PR #122 was
inspected with `gh pr view`, `gh pr diff`, and its git history as evidence only;
no code was transplanted wholesale and the PR was not modified.

At the start of the work:

- PR #122 was an open draft at
  `9be0b06dc4317145c88c46742f8f918c64422883`, with no reviews, comments,
  linked issues, or recorded dependencies.
- PR #135 was an open draft at
  `d750d9ee3a90132657e653f678ab35149b16b1a7`. It mechanically overlaps worker
  supervision, Python session/FFI, IPC, tests, and architecture docs, but has
  no intended semantic dependency.

Both PRs were re-queried immediately before publication and remained open
drafts at the same heads. Their current changed-file sets confirm that #135
still mechanically overlaps architecture/docs, IPC, Python session/FFI, and
worker supervision without creating a semantic dependency.

The surviving history does not record one exact regression that caused PR
#122 commit `9f6af64e` to return from ETX to targeted `CTRL_BREAK_EVENT`. The
commit has no explanatory body, and the parent CI failures are unrelated
restart-output assertions. The supportable conclusion from the diff is:

- `9f6af64e` bundled broad Windows/ConPTY stabilization and replaced direct ETX
  with targeted `CTRL_BREAK_EVENT` plus R/Python runtime-state injection.
- Follow-up `8e4f9d3` broadened the Zod interrupt fixture from Ctrl-C to
  Ctrl-Break, confirming the transport change.
- A narrower causal story would be inference, so this plan does not invent one.

## Source Investigation

Exact source revisions inspected:

- reticulate 1.46.0:
  `cf729e978aaf3c08a87899604be5ec4a3985d982`
- R 4.5.3:
  `c5ddd2fcc67d751f51085e5a29f8158410fc0eaf`
- CPython 3.14.5:
  `5607950ef232dad16d75c0cf53101d9649d89115`
- Microsoft Terminal:
  `4f225a56aff245bbb9d1400f266c20e1747cc580`

The inspected runtime paths were:

- reticulate `src/python.cpp`, including its Windows console-handler chain;
- R `src/gnuwin32/psignal.c::hwIntrHandler` and
  `src/main/errors.c::R_CheckUserInterrupt`;
- CPython `Modules/signalmodule.c::trip_signal`; and
- CPython `Parser/myreadline.c::PyOS_Readline`, including its GIL-release
  boundary.

The inspected reticulate handler removes and re-adds itself while handling an
event. This confirms that mcp-repl's observer must re-register before every
managed wait and before publishing readiness, so it remains the newest handler.

CPython's `trip_signal()` establishes pending state before waking the main
thread. Its source comment describes the same wake-before-flag race avoided by
the completion barrier here. R likewise requires the runtime main thread to run
`R_CheckUserInterrupt()`.

CPython's `PyOS_Readline()` calls its installed callback inside
`Py_BEGIN_ALLOW_THREADS`. The Windows native-`input()` path therefore reaches
mcp-repl without the GIL, while direct `_mcp_repl` managed-read callbacks still
hold it. Blocking observer operations preserve that caller distinction and
never attempt a nested GIL release.

Microsoft Terminal maps ETX to Ctrl-C input and produces `CTRL_C_EVENT` when
`ENABLE_PROCESSED_INPUT` is enabled. Its control-event dispatch records/queues
delivery before handler execution; delivery initiation is not completion.

## Handler-Completion Proof

A throwaway standalone Windows probe registered:

1. a consuming handler that signals "entered", blocks on a Windows event, sets
   a marker, and returns `TRUE`; then
2. the proposed newest-first observer.

The observer duplicated the pseudo-handle from `GetCurrentThread()` into a real
waitable handle, published it through a bounded handoff, and returned `FALSE`.
For each of two sequential native Ctrl-C events:

- the duplicated thread handle returned `WAIT_TIMEOUT` while the consuming
  handler remained blocked;
- after the test released the handler and it returned `TRUE`, the handle became
  signaled.

The probe passed twice with:

```text
cargo test -j 1 --test windows_ctrl_handler_completion_probe -- --test-threads=1 --nocapture
```

The first probe iteration also exposed inherited Ctrl-C-ignore behavior. The
isolated console needed `SetConsoleCtrlHandler(NULL, FALSE)` before handlers
could run. Production initialization clears that inherited state.

The throwaway probe was removed after the public regressions covered the
contract. The result proves that termination of the Windows-created handler
thread orders the complete process-local handler chain, including a later
handler returning `TRUE`.

Immediate wake from the observer is not sound: the runtime main thread could
check its state before a later runtime/package handler sets it. Waiting for
handler-thread termination supplies the required barrier without interpreting
the interrupt.

Handler lists and `TRUE`/`FALSE` propagation are process-local. A handler in a
child process sharing the console cannot consume the worker process's event or
replace the worker-local completion observer.

A third process would therefore add no useful ordering boundary: console
handlers and `TRUE` propagation are worker-process-local, while the existing
worker sideband reader already owns queued-input cleanup. A worker-local
observer plus its pre-existing watcher thread is the necessary boundary.

## Public Red Tests

`tests/windows_native_interrupt.rs` exercises the public MCP `repl` surface. Its
Python setup installs a real `SetConsoleCtrlHandler` callback with `ctypes`,
uses named Windows events to gate entry and release, records the native event
code, and covers both consuming-handler policies. Its R cases cover execution
and managed `ReadConsole`/`readline` interruption plus later session use.

Before production changes, on current `origin/main`:

```text
cargo test -j 1 --test windows_native_interrupt -- --test-threads=1 --nocapture
```

failed because:

- Python's native callback never entered, so the required `CTRL_C_EVENT` was
  absent; and
- R's managed-input case did not report the ordinary interrupt condition.

The Python regression was independently red for the contract: main delivered
targeted `CTRL_BREAK_EVENT`, sideband code synthesized runtime interruption, and
cached readiness could settle before a blocked native handler finished.

## Native Delivery Selection

Both candidates were exercised in a temporary public-process probe, rerunning:

```text
cargo test --test windows_ctrl_delivery_probe windows_native_ctrl_delivery_probe -- --exact --nocapture --test-threads=1
```

The ConPTY variant reported `CONPTY_ETX_OK`: exact ETX reached processed ConPTY
input and the worker received event code `0` (`CTRL_C_EVENT`).

Attaching to the worker console and calling
`GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)` could also initiate delivery, but
was rejected for production. `CTRL_C_EVENT` cannot target a nonzero process
group, so the sender must attach/detach around the dedicated console and
temporarily protect itself. That is more invasive than using the already-owned
ConPTY input boundary. A successful `GenerateConsoleCtrlEvent()` call would
still prove only delivery initiation, not handler completion.

Production ships only ConPTY ETX. There is no `CTRL_BREAK_EVENT`, direct
generation fallback, or runtime-state fallback.

## Production Design

### Worker-local observer

`src/windows_interrupt_observer.rs` is shared by built-in R and Python workers.
Before every managed wait and before readiness is emitted, it re-registers its
handler newest-first.

Initialization publishes handles and starts the watcher before registering the
callback, so a callback can never hand off without an existing consumer.

The callback:

- handles only `CTRL_C_EVENT`;
- duplicates the current Windows-created handler thread into a real handle;
- publishes the handle through a bounded atomic/event handoff to an existing
  watcher;
- returns `FALSE` immediately;
- performs no Rust allocation, logging, mutex acquisition, or runtime call.

The watcher waits for handler-thread termination and signals the runtime main
thread. Setup, overlap, handoff, and wait failures surface as worker protocol
errors. Completion wins when input and interrupt completion are both ready.

Rearming alternates between two callback entry points. The new entry point is
registered before the old one is removed, so there is no interval without an
observer. If dispatch overlaps that brief registration window, the duplicate
entry on the same Windows handler thread is a no-op; an actually concurrent
dispatch on a different handler thread still fails closed.

Each alternating wrapper volatile-reads a distinct retained identity token, so
release-link identical-code folding cannot merge their addresses. Initialization
also fails closed if the callback addresses nevertheless compare equal.

The complete rearm swap is serialized outside the handler with a process-local
mutex. Runtime-main readiness, background managed-stdin publication, and the
sideband preparation path therefore cannot interleave their add/store/remove
steps. The handler and watcher never acquire this mutex.

A package may install a newer consuming handler while user code is running.
The server therefore does not write ETX until the sideband thread has
re-armed the observer and returned a pipe-ordered `interrupt_armed` fact.

### Cleanup and runtime checkpoint

Sideband `interrupt` remains cleanup-only. The sideband reader discards queued,
unconsumed input while preserving a live managed-input consumer, and records
cleanup completion, but does not wake managed input as an interrupt and does
not set runtime state.

The runtime main thread joins native handler completion with cleanup completion.
R emits payload-free `interrupt_complete` immediately before
`R_CheckUserInterrupt()` because that checkpoint may longjmp. Python reacquires
the GIL, calls `PyErr_CheckSignals()`, then emits `interrupt_complete` and clears
pending state under its readiness-publication barrier. Python completion is
owned by the saved runtime main thread even when a background Python thread is
blocked in managed stdin.

Python uses a dedicated publication mutex with one lock order around cleanup,
pending-state transitions, and the final readiness write. A background
publisher that reaches the pre-commit boundary first writes before cleanup;
cleanup that reaches the boundary first forces it to defer through the runtime
checkpoint. Runtime-main `ready` is atomically suppressed while the preserved
background reader remains active, so the reader republishes `input_wait`
without allowing the next answer to be misclassified as a cell.

### Evidence-driven protocol expansion

The initial implementation tried to retain existing message shapes and use
fresh readiness alone. Two deterministic public races disproved that design:

- A zero-timeout Ctrl-C could return while its native handler was still
  blocked, and cached readiness could admit later input before the runtime
  checkpoint.
- A package could install a newer consuming handler during a cell. If Ctrl-C
  was written while the runtime was between cell finish and observer rearm,
  that handler returned `TRUE` before the older observer ran. The server then
  had no handler-thread completion fact to join.
- A background managed-stdin publisher could pass a pending-state check, pause,
  and otherwise write stale readiness after the runtime checkpoint marker. Its
  cleanup also exposed that clearing live reader ownership could route the next
  answer as a new cell.

That evidence justified two narrow additive worker-to-server messages:

```json
{"type":"interrupt_armed"}
{"type":"interrupt_complete"}
```

Neither is an interrupt request, input ID, or runtime authority.
`interrupt_armed` is a bounded pre-delivery acknowledgment that the existing
cleanup-only request has been processed and the observer is newest.
`interrupt_complete` is emitted on the runtime main thread only after handler
termination and cleanup have joined. These are transaction-specific ordering
facts, not a general readiness or discard-ack redesign.

Adding these strict message variants changes the worker protocol from version 6
to version 7. Older workers are rejected; there is no compatibility path.

The transaction is opened before cleanup or ETX delivery. The server sends the
cleanup-only request, waits without polling for `interrupt_armed`, and only
then writes ETX. It settles the transaction after `interrupt_complete` and a
pipe-later `input_wait` or `ready`. Stale readiness cannot settle the original
request or admit later worker-bound input. Overlapping Ctrl-C requests
coalesce while one native dispatch is outstanding. Process replacement clears
the transaction.

### Launch boundaries

Built-in R and Python workers use dedicated ConPTY input for native Ctrl-C.
Processed input is enabled and inherited Ctrl-C-ignore behavior is cleared.
Nested Windows sandbox launch forwards ConPTY input bytes unchanged so ETX
reaches the inner console.

Moving built-in R onto ConPTY exposed two terminal-envelope details which are
kept separate from runtime output: the exact ConPTY startup mode toggles and
the exact console-reset sequence emitted during shutdown are removed only from
the built-in worker's raw terminal stream. Matching is byte-exact and
split-read tolerant; later user/runtime terminal bytes pass through unchanged.
Windows emits both a plain cursor/screen reset and a mode-reset/title-frame
variant. A complete candidate that races ahead of the sideband `session_end`
callback is held until that callback arms the filter; an unarmed candidate is
restored during finalization. This preserves identical user output while
removing only a lifecycle frame joined to the session-end boundary. R shutdown
writes console EOF before waiting but leaves the ConPTY open until R returns,
so `.Last` and other cleanup output remains ordered before `session_end`.

A custom Windows worker using ConPTY receives ETX and owns its own CRT
`CONIN$`, processed-input, native-handler, and completion policy. A pipe-only
custom worker has no isolated native Ctrl-C delivery boundary and fails its
interrupt request explicitly. mcp-repl does not pretend that sideband cleanup
is native interruption.

The dedicated ConPTY input is server-owned. A native Ctrl-C event not paired
with the server's cleanup transaction is outside the built-in contract; there
is no polling or synthesized-state fallback for it.

PTY/ConPTY raw capture is one terminal stream and therefore does not preserve
raw stdout/stderr identity. Worker-owned sideband `output_text` retains its
declared stream.

## Validation Evidence

Targeted public native suite:

```text
cargo test -j 1 --test windows_native_interrupt -- --test-threads=1 --nocapture
```

passed all 12 discovered tests: 11 functional cases plus the shared transcript
unit. Coverage includes two sequential `CTRL_C_EVENT` deliveries, overlapping
zero-timeout Ctrl-C coalescing, separate and same-call tail admission,
pre-admission timeout without session replacement, finish-to-rearm ordering,
background runtime-main checkpoint ownership, the readiness-publication
commit race, consumer ownership preservation, consuming handlers with and
without Python interrupt state, startup failure diagnostics, and R recovery.

The read-only and workspace-write cases both passed with unrestricted base-token
execution. Each uses an isolated temporary workspace so the public interrupt
regression tests the sandbox policy and nested ConPTY forwarding without making
the fixed IPC-connect budget depend on the checkout's accumulated `target/`
size.

The actual reticulate 1.46.0 regression used an isolated library and explicit
Python runtime:

```powershell
$env:MCP_REPL_RETICULATE_LIBRARY='C:/tmp/mcp-repl-reticulate-lib'
$env:MCP_REPL_RETICULATE_PYTHON='C:/Users/kalin/AppData/Local/Python/bin/python.exe'
cargo test -j 1 --test windows_reticulate_interrupt reticulate_handler_interrupts_r_input_without_stale_python_state -- --exact --test-threads=1 --nocapture
```

It passed. After Ctrl-C, the R follow-up evaluated to `42`, the reticulate
Python follow-up evaluated to `42`, and no stale `KeyboardInterrupt` remained.

Focused protocol tests also passed:

```text
cargo test --lib interrupt_armed_is_payload_free_worker_acknowledgement -- --nocapture
cargo test --lib interrupt_complete_without_pending_transaction_is_protocol_error -- --nocapture
cargo test --lib duplicate_interrupt_complete_is_protocol_error -- --nocapture
cargo test --lib interrupt_arm_acknowledgement_wakes_bounded_waiter_and_resets_per_transaction -- --nocapture
cargo test --lib pending_interrupt_gates_stale_request_readiness_until_joined_later_readiness -- --nocapture
cargo test --lib interrupt_observation_requires_later_pipe_ordered_readiness -- --nocapture
cargo test --lib interrupt_complete_is_payload_free_worker_observation -- --nocapture
```

The custom-worker ConPTY payload-free interrupt case and pipe-only explicit
rejection case passed. `cargo test --test docs_contracts` passed all 20 tests.
One focused protocol invocation was transiently blocked by Windows Application
Control (`os error 4551`); the immediate exact retry passed.

The final source passed:

```text
RUSTFLAGS=-Dwarnings cargo check
RUSTFLAGS=-Dwarnings cargo build
python3 tests/run_integration_tests.py --binary target/debug/mcp-repl
cargo clippy --all-targets --all-features -- -D warnings
cargo +nightly fmt
RUSTFLAGS=-Dwarnings cargo build --release --locked
git diff --check
```

The integration runner passed all 21 applicable scenarios. The final
`RUSTFLAGS=-Dwarnings cargo test --quiet` run passed all 478 library tests and
every discovered integration binary, including the 12-test Windows native
suite. The actual reticulate regression passed separately with its explicit
library and Python runtime. The final unrestricted run completed without Code
Integrity interruption.

## Relationship To Other Work

- PR #122: this supersedes its Windows interrupt implementation. It does not
  carry forward nullable prompts, broad `ready` redesign, runtime-specific
  interrupt injection, input IDs, or a general discard-ack redesign. The two
  narrow protocol ordering facts above are directly public-regression-driven.
  Once this replacement lands, #122 should not be merged as-is.
- PRs #130 and #131: preserve the worker-owned sideband input queue and single
  input owner. Process stdin is not restored as accepted-input transport.
- PR #132: preserve Python's prompt-free cell/readline distinction.
- PRs #1 and #123: extend their experimental Windows/ConPTY support and test
  parity; do not supersede them.
- PR #135: mechanical file overlap exists, but there is no semantic dependency.
  Whichever PR lands second will need to rebase.

## Locked Decisions

- Public MCP regressions, not an internal observer unit test, define success.
- Runtime and package native handlers remain authoritative.
- Handler-thread termination is the completion barrier.
- Observer registration precedes published readiness.
- ConPTY ETX is the only built-in Windows native delivery mechanism.
- Sideband cleanup and native control delivery remain separate channels.
- Payload-free pre-delivery arm and post-handler completion facts are the only
  protocol expansion.
- No sleep conceals handler, cleanup, readiness, or admission ordering.
- No polling, fallback delivery chain, or third process is permitted.
- Pipe-only custom Windows interrupt requests fail explicitly.
- PR #122 is not modified, force-pushed, or closed by this work.

## Open Questions

- None.

## Next Safe Slice

- Publish the draft PR and monitor GitHub Actions on its clean Windows runner.
- If hosted checks expose a branch-specific failure, fix it in scope and update
  this completed record.

## Stop Conditions

- If final validation disproves handler-thread completion or deterministic
  ordering, stop rather than adding polling, a fixed delay, or runtime-state
  injection.
- If a launch mode lacks a dedicated native console boundary, keep the explicit
  unsupported result rather than silently degrading to sideband interruption.

## Decision Log

- 2026-07-24: Scoped the work to a focused Windows native-interrupt slice and
  treated PR #122 as history rather than an implementation source.
- 2026-07-24: Proved duplicated handler-thread termination orders a blocked,
  consuming handler returning `TRUE`, twice sequentially.
- 2026-07-24: Confirmed public regressions fail on current main.
- 2026-07-25: Selected exact ETX over the dedicated ConPTY input. Rejected
  direct console attach/generation as more invasive and not a completion
  barrier.
- 2026-07-25: Kept sideband interrupt handling cleanup-only and moved R/Python
  checkpoints to their runtime main threads.
- 2026-07-25: Added `interrupt_complete` only after the public blocked-handler
  tail-input regression proved cached/fresh readiness alone was insufficient.
- 2026-07-25: Required server-side coalescing and input admission gating while
  the marker-plus-readiness transaction remains outstanding.
- 2026-07-25: Closed the remove-then-add observer gap by alternating callback
  entry points and registering the replacement before removing the previous
  entry.
- 2026-07-25: Serialized the complete rearm swap after an independent audit
  found that runtime-main and background Python publication could otherwise
  interleave alternating registrations.
- 2026-07-25: Preserved the existing CPython readline GIL contract after the
  public background-stdin test exposed an invalid second GIL release.
- 2026-07-25: Added a pipe-ordered pre-delivery arm acknowledgment after the
  public finish-to-rearm race proved that a package's newer consuming handler
  could otherwise hide the dispatch from the observer. The server gates stale
  request completion as well as later input until the full transaction settles.
- 2026-07-25: Kept built-in R cleanup output intact under ConPTY by sending
  console EOF before shutdown wait, and filtered only the exact ConPTY
  startup/shutdown control sequences from raw capture.
- 2026-07-25: Corrected the ConPTY lifecycle grammar from one assumed reset
  ordering to the two exact Windows-emitted variants. Buffered complete frames
  until the sideband session-end boundary so cross-pipe scheduling cannot leak
  terminal control bytes or suppress identical unarmed user output.
- 2026-07-25: Ran the native sandbox regression in isolated temporary
  workspaces after tracing a workspace-write IPC timeout to ACL refresh over
  the repository's large generated `target/` tree; both real policies then
  passed without skips or timeout changes.
- 2026-07-25: Re-queried #122 and #135 at their unchanged open-draft heads and
  confirmed #135's remaining overlap is mechanical only.
- 2026-07-25: Preserved startup observer diagnostics as a terminal
  diagnostic-bearing first `session_end` and allowed the Windows named-pipe
  connector to accept a pending connection from a worker that exits quickly.
- 2026-07-25: Restored an incomplete lifecycle-control prefix at ConPTY EOF so
  split matching never suppresses user output.
- 2026-07-25: Kept CRT stdin read-only after an empirical Windows access-denied
  result by using a separate, short-lived read/write `CONIN$` handle only for
  the processed-input mode update.
- 2026-07-25: Completed the warning-denied repository matrix: 478 library
  tests, every integration binary including all 12 Windows native cases, the
  actual reticulate regression, clippy, formatting, docs contracts, integration
  runner, and locked release build all passed.
