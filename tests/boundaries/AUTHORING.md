# Authoring a boundary case

Find a related case with `scripts/test --locate SELECTOR`, read its contract, and add a public regression before changing behavior.
Follow the [validation ladder](../../docs/DEVELOPMENT.md#validation-ladder).

## Readable session transcripts

A transcript should read as a plausible session or focused regression.
Extend existing cases instead of adding duplicate launches for permutations.
Before removing a case, identify surviving coverage and remove its producer and snapshots together.
Check schema/configuration through public discovery or configuration.

Use real runtimes for ordinary output, plots, validation, and adapters.
A synthetic worker needs a failure mode that real runtimes cannot reproduce reliably; explain that need in the fixture description.
Preserve real-library sandbox cases and distinct cold-cache, activation, isolation, and permission paths.

Record actual exchanges, not synthetic summaries of calls.
Keep adapter return-value checks in Python assertions while retaining their underlying requests/responses.
Prefer equality assertions on complete structured results.
Reuse shared helpers; do not add production APIs just to inspect private state.

Make YAML understandable without the producer.
Add short `scenario` and `fixture` descriptions for fake workers, hidden runtimes, injected failures, gates, or host intervention.
Keep that prose in the producer.
Use one Console invocation per transcript and companions for distinct sessions.
Pass child environments explicitly instead of changing the test runner's environment.

## Embedded programs

Use short strings for short programs and dedented multiline literals for longer ones.
Put `# fmt: python` or `# fmt: r` immediately above `code(` and its opening delimiter, including in nested calls.
Indent payload and closing delimiter one level deeper:

```python
client.send(
    # fmt: python
    python=code(r"""
        for value in (1, 2):
            print(value)
        """),
)
```

Use raw literals when backslashes belong to the program.
Preserve its indentation and trailing newlines.
Do not label a custom worker's command language as R/Python just because it travels in that field.
Run `scripts/format` and inspect all results; `--strict` fails after attempting every formatter if one is missing or fails.

## Execution modes

Use `@executions(RUNTIME)` for ordinary language or representation behavior and `@executions(DIRECT, SANDBOXED)` when the contract differs by mode.
Launch through `execution.serve()`.
Put sandbox-only arguments in explicit fixtures such as `SANDBOXED.serve("--writable-root", str(root))`, with the appropriate capability.
See [requirements and modes](README.md#requirements-and-execution-modes).

## Lifecycle receipts and checkpoints

Establish the observed state an assertion needs before releasing the fixture.
Sleeps, broad matching, and longer timeouts do not establish ordering.
A marker or stdin write does not prove public admission; a response frame does not prove its delivery acknowledgment has settled; an accepted interrupt does not prove completion.

```python
client.send(python=program, timeout_ms=0)
assert last_tool_text(client) == "\n[running; poll with an empty send]"
started.wait("worker reached the release gate")
release.release()
```

Use [checkpoints](../support/checkpoints.py) for marker/FIFO gates, [capture](../support/capture.py) for complete stream batches, and [Events](../support/events.py) for process/filesystem observation and cancellation.
Keep cleanup in context managers or `finally`.
Start liveness deadlines at the operation's checkpoint, after unrelated discovery/preparation has finished.
Use `McpClient.response_deadline()` for waits that must preserve shutdown/reap time.

Choose a receipt at the boundary being asserted.
Before competing with a gated submission, observe its polling/admission claim.
Before testing EOF delivery, gate an accepted result.
Before checking shutdown output attribution, observe cell completion and its recorded output event.
Settle preceding response writes with a ping before racing a long poll.
Integrator callers requiring idle state must observe `status.idle` or use ordinary `busy: "wait"` admission.

Replies do not wait for the recording writer.
After each reply following the first `send`, `McpClient` waits until the live recording holds that reply's `delivery` event.
The writer appends the event after everything the reply recorded, so a case can read recording files as soon as it has the reply.
Clear `client.recording_receipts` around a reply whose stdout write or recording write the case holds, and pass `recording_receipts=False` when the case stalls or disables the writer.
When a held reply write blocks every later reply, use `client.await_received()` to prove the server applied an earlier message and `client.await_assembled()` to prove a later handler finished.
`pause_results` holds a `tools/call` reply at its stdout write, after its result is assembled.
`cancel_result` cancels the call there, proves the cancellation with the recorded receipt of a following ping, and then receives the reply the client no longer wants.

Raw streams and sideband are independent.
Use `read_relay_ready()` for the ordered acquisition/readiness handshake and count every record up to the producer's gate.
Observe sideband cell entry in the capture before releasing raw output whose attribution depends on it.
Read active journals through `live_recording_events()`; parse finalized journals in full so truncation remains an error.

`wait_for_evaluation_output()` submits once, retains each output delta, and bounds collection.
`client.expect()` checks exact successful output through that helper.
Use `controlled_completion=True` for manually collected control-and-cell calls; their final `[done]` marker still matters after earlier polls took the text.
`client.expect()` selects this automatically for its own combined submissions.

Give cold preparation an explicit budget and establish completion before dependent calls.
An observation timeout or code-plus-stdin response can arrive before setup or input consumption finishes.
Use explicit pending-state assertions when a running or input-blocked cell is the contract.
Retain raw exchanges when delivery/polling is being tested.
Local wheels suit import/install sentinels; use real libraries when their behavior or permissions matter.

See [Python lifecycle](client_server/python/test_lifecycle.py), [output retention](client_server/output/test_spools.py), and the [snapshot rules](README.md#snapshots).
