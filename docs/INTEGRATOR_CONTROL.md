# Internal integrator control channel

Applications can inspect and evaluate Console sessions through a local socket while an agent uses MCP.
This interface is experimental and has no compatibility guarantee; use the protocol matching the Console build.
The hidden `--control-socket` option is available on macOS/Linux, disabled by default, and unavailable on Windows.

Control and MCP requests for the same `session` share its worker, objects, requirements, and permissions.
Names select separate workers in the same registry and capacity pool.
The channel can use a configured runtime hidden from the model's tool schema, but cannot grant additional permissions.

## Start the socket

Pass `--control-socket PATH` to `mcp-console serve`.
The parent directory must already exist, belong to the current user, and have mode `0700`.
Console creates a mode-`0600` socket and refuses any existing path, including a stale socket.

```python
import json
from pathlib import Path
import socket
import tempfile

from mcp_console import MCPConsole

with tempfile.TemporaryDirectory(prefix="console-control-") as directory:
    path = str(Path(directory) / "control.sock")
    with MCPConsole(
        args=["serve", "--control-socket", path, "-c", "languages=[python]"]
    ) as console:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as control:
            control.connect(path)
            with control.makefile("rwb") as messages:
                request = {
                    "id": "setup",
                    "op": "evaluate",
                    "language": "python",
                    "code": "selected_region = 'North'",
                    "busy": "wait",
                    "wait_ms": 60000,
                    "record": False,
                }
                messages.write(json.dumps(request).encode() + b"\n")
                messages.flush()
                print(json.loads(messages.readline()))
        # Register Console tools while this MCPConsole connection remains open.
```

MCP owns the server lifetime.
Closing a control connection leaves the server running; MCP shutdown retires all workers, closes control connections, and removes the socket.
With inherited client permissions, a control evaluation returns `not_ready` until MCP has captured that authority.

## Send requests

Send one UTF-8 JSON object per newline-terminated frame, up to 1 MiB including its newline.
Each request requires a string `id` and an `op`.
Unknown fields, operations, and incorrect types are rejected.
Oversized or unterminated frames close the connection.
[server/control.rs](../src/server/control.rs) owns the schema.

Wait for a response before sending another request on that connection.
Use another connection for inspection while an evaluation waits or runs.
Responses carry the same `id` and either `result` or an `error` with `code` and `message`.
Malformed requests use `id: null`.
IDs correlate requests; they do not deduplicate execution.

```json
{"id":"state","op":"status"}
{"id":"state","result":{"state":"idle","idle":true,"evaluation":null}}
```

Except for `sessions`, operations accept a `session` string, defaulting to `default`; `create` and `close` require it explicitly.

| Operation      | Fields beyond `id`, `op`, and `session`                                                 | Purpose                                                                     |
| -------------- | --------------------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| `sessions`     | None                                                                                    | Inspect names, lifecycle states, and typed-output worker identities.        |
| `create`       | None                                                                                    | Configure a named session lazily.                                           |
| `close`        | None                                                                                    | Stop a non-default worker while retaining its name and requirements.        |
| `status`       | None                                                                                    | Inspect current state and ordinary evaluation identity.                     |
| `output`       | None                                                                                    | Read retained typed output.                                                 |
| `preview`      | None                                                                                    | Read pending text/images without collecting them.                           |
| `requirements` | None                                                                                    | Inspect accepted declarations, preparation state, and runtime requirements. |
| `manifest`     | None                                                                                    | Read the accepted requirements declaration alone.                           |
| `prepare`      | `requirements` with `action: "add"` (the default)                                       | Prepare additions without executing a cell or restarting.                   |
| `recording`    | None                                                                                    | Get existing recording and projection paths.                                |
| `evaluate`     | `language`, `code`; optional `execution`, `busy`, `wait_ms`, `record`, `include_output` | Run an ordinary or immediate evaluation.                                    |
| `subscribe`    | Optional `format`: `preview` (default) or `events`                                      | Receive replacement snapshots on a dedicated connection.                    |
| `stdin`        | `evaluation_id`, `input`                                                                | Send input to the identified active evaluation.                             |
| `interrupt`    | `evaluation_id`                                                                         | Interrupt that evaluation.                                                  |
| `restart`      | None                                                                                    | Retire and replace the selected worker.                                     |

`language` is `r`, `python`, or `sql`; `code` is a string.
Evaluation and preview results contain `resultType: "complete"`, `content`, and `isError`.
Language diagnostics remain in content; `isError` marks server/worker operation failures.
Evaluations also return `execution_error`: null or a bounded object containing `ename`, `evalue`, and `traceback`, with `truncated: true` when shortened.
Admission failures use the outer `error` field.
Source owns runtime-specific error classification.

Use `status.evaluation.id` for input and interrupts.
Stale IDs are rejected so delayed controls cannot affect a successor.
Startup input is unsupported through this channel.
MCP controls for the same name still affect the shared session.

### Sessions and preparation

`sessions` observes without starting workers or collecting output.
Entries contain `name`, `state`, and `worker_id`; states are `starting`, `running`, `waiting_for_stdin`, `idle`, or `stopped`.
Stopped names remain reusable.

`create` uses captured configuration and normal preparation/capacity admission, leaving interpreter startup to the first ordinary cell.
A new name returns `{"session":"analysis","state":"stopped","worker_id":0}`.
Creating an existing name does not clear state or requirements.
Ordinary evaluation can also create a name; inspection and immediate execution cannot and return `unknown_session`.

`close` waits for retirement before releasing capacity and preserves pending MCP output and accepted requirements.
Closing `default` is rejected.
`restart` waits for retirement and replacement startup, returns `accepted` and the new `worker_id`, and clears runtime objects.
Rebuild application setup for that identity.
Control restart starts a replacement for every name; MCP restart without code leaves a named worker stopped.
Both operations continue after admission if the socket disconnects, and private evaluations still settle with their original callers.

`prepare` returns `not_ready` until configuration and client authority are available.
It then requires idle admission with no pending evaluation, bootstrap, or immediate request; otherwise its result is `status: "busy"`.
A stopped named session can accept prestart preparation after confirmed retirement.
Once admitted, it continues after disconnection.
Results use `prepared`, `restart_required`, `busy`, or `failed` and include `worker_id`, `requirements`, `prepared`, and `runtime_requirements`.
These describe the accepted state, including partial activation after a mixed-language failure.
Failure messages are bounded and mark truncation.
Replace/reset remain MCP operations.

`requirements` and `manifest` do not prepare packages or run code.
They return `not_ready` before configuration is available.
Declarations are not package inventories or lockfiles.
Control receipts do not collect pending MCP output or shared preparation diagnostics.

`recording` returns absolute `directory`, `markdown`, and `qmd` paths plus a `worker_id`.
QMD is null until the selected worker has recorded source or notes; unavailable Markdown produces `projection_unavailable`.
Like other control requests, it activates recording.
Its `worker_id` is the recording's global identity, distinct from the per-name typed-output identity used by other operations.

## Choose how to handle a busy worker

Ordinary evaluations default to `execution: "serial"`:

| Policy                     | Behavior                                                                                                 |
| -------------------------- | -------------------------------------------------------------------------------------------------------- |
| `busy: "reject"` (default) | Return `busy` if admission is unavailable; omit `wait_ms`.                                               |
| `busy: "wait"`             | Require integer `wait_ms` from 0 through 4294967295; return `timeout` if admission misses that deadline. |

The deadline covers startup and admission, not execution.
Pending MCP output can hold admission until the MCP client collects it.
Use `busy: "wait"` after an MCP response because local delivery acknowledgment may still be settling.
Waiting does not reserve queue position; replacement invalidates a wait for the old worker.

Disconnection before admission cancels submission.
After admission, evaluation continues and remains privately owned: its result does not become MCP output, although it can change objects later agent calls observe.

### Opt in to immediate execution

`execution: "immediate"` runs a short private R or Python request in the existing worker.
SQL is unsupported.
It requires `busy: "reject"` and no `wait_ms`.
Initialize the requested interpreter and prepare dependencies through ordinary cells first.
Only one immediate request can be outstanding per worker; it cannot prepare packages or request managed stdin.

| Runtime | Execution and constraints                                                                                                                                                                                                                        |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| R       | Reenters the owning thread at an event checkpoint, including `Sys.sleep()`. Cannot preempt native code that does not service R events. Returns `busy` while a non-R ordinary cell is admitted, or an admitted R cell has not entered the worker. |
| Python  | Uses a background thread and requires the GIL. Native code holding the GIL can delay it. Background Python must not call R.                                                                                                                      |

Ordinary admission continues while immediate R runs, but dispatch waits for R to return and remains cancellable.
Keep private requests short and tolerate `busy`; they cannot take precedence over an already admitted ordinary cell.
A restart can fail admitted immediate work; it never replays it on the replacement.

Immediate R evaluates in the global environment while preserving ordinary REPL history, `.Last.value`, task callbacks, and foreground warning/error state.
Private errors do not invoke ordinary `options(error)`.
An uncaught interrupt remains pending for the foreground after private cleanup.
Integrators remain responsible for safe concurrent object access and for interrupts their code catches.

After initializing Python:

```json
{
  "id": "ui",
  "op": "evaluate",
  "language": "python",
  "execution": "immediate",
  "code": "print(2 + 2)"
}
```

Managed output belongs to the private result, including Python thread cleanup.
Native stdout/stderr and child output cannot identify a writing thread and retain foreground attribution.
Graphics state is shared; immediate calls do not close foreground devices or automatically finalize plots.
Explicit images retain their call/worker owner without creating an ordinary numbered cell interval.

## Inspect state and pending output

`status` reports ordinary cell ownership, not immediate work.
Its current state can describe startup, preparation, evaluation, input, completion, pending delivery, restart, shutdown, failure, or idle.
Inspection can return `busy` if state cannot be read immediately.
The response does not reserve the worker.

`preview` reads bounded pending content, including partial terminal lines, without consuming output, acknowledging delivery, or changing omission accounting.
A preview never waits for the recording writer.
Its omission notice includes a warning described in [Recording failures](RECORDING_REFERENCE.md#recording-failures) only if recording has stopped or a backlog that a reply's bounded wait gave up on remains.
Output already claimed by a response writer may no longer be visible there.

### Subscribe to pending output

Send `{"id":"ghost","op":"subscribe"}` on a dedicated connection.
After `{"id":"ghost","result":{"subscribed":true}}`, frames use this shape:

```json
{
  "id": "ghost",
  "event": "preview",
  "result": {
    "resultType": "complete",
    "content": [{ "type": "text", "text": "working…" }],
    "isError": false
  }
}
```

Replace the displayed content with each snapshot, including an empty one after collection.
A new snapshot is also sent when the recording writer changes an omission notice about retained files, even if the worker emits no output.
Slow readers can miss coalesced changes or output already collected by MCP.
Subscriptions retain only the latest change and never block output producers.
Close the connection to unsubscribe; use other connections for requests.

### Inspect typed output

`output` returns bounded history captured before terminal rendering and MCP assembly.
It survives ordinary MCP polling/collection and is retained only while the control listener is enabled.
[output/events.rs](../src/worker_client/output/events.rs) owns its exact serialized shape and retention constants.

Snapshots include `worker_id`, `first_sequence`, `next_sequence`, omission counts, `evaluations`, and `events`.
Evaluation descriptors carry identity, origin, language/source, execution mode, control request/recording call associations, settlement state, confirmed entry, and bounded error information.
`complete` means settled, including failure or cancellation before entry; it does not acknowledge MCP delivery.
`started` distinguishes admission from actual ordinary execution.

Each event carries `sequence`, nullable `evaluation_id`, and its type:

| Types                                                               | Payload                                  |
| ------------------------------------------------------------------- | ---------------------------------------- |
| `stdout`, `stderr`                                                  | Decoded raw process text.                |
| `output`, `diagnostic`                                              | Managed console text.                    |
| `notice`                                                            | Server-generated information.            |
| `image`                                                             | Encoded `data` and `mimeType`.           |
| `execution_error`                                                   | Structured error and retained traceback. |
| `evaluation_admitted`, `evaluation_started`, `evaluation_completed` | Evaluation lifecycle.                    |
| `omission`                                                          | An image too large for history.          |

Text preserves terminal controls and partial lines.
Raw bytes are incrementally decoded with replacement for invalid UTF-8; this is not a byte-for-byte recording.
Sequences reflect observation order, without cross-stream chronology.
Output between ordinary admission and completion carries that evaluation ID; idle output has null identity.
Native output from immediate work retains foreground attribution.

Replacement increments the selected name's `worker_id` and clears its history.
Identify output by session name plus worker ID; different names can share a number.
Evaluation IDs increase across the server; sequences belong to each output owner.
Retention is limited to 512 events, 64 KiB of text/error payloads, 8 MiB of encoded images, and 32 evaluation descriptors.
Descriptors bound source to 8 KiB and request IDs to 512 bytes, with truncation flags.
Evicting a descriptor also evicts its events.
Expose omission counters; a retained tail is not complete output.

Subscribe with `{"id":"output","op":"subscribe","format":"events"}` for these snapshots under `event: "output"`.
Subscriptions follow the selected name across replacement.
Named frames include `session`; omission means `default`.
Both inspection and subscription return replacement snapshots, not event batches.
Reconcile by identities/sequences rather than appending the same output twice.

`include_output: true` on evaluation adds `evaluation_id` and `output` to its response.
Serial results filter shared history to that evaluation, while bounds and omission counts still describe the whole history.
Immediate results use a private tape visible only to the requester.
Ordinary `record: false` calls omit their observed interval/source from shared history as well as document projections.

## Select recording projections

Control calls/results enter the journal; ordinary evaluations also retain source, execution boundaries, raw output, and images.
Immediate calls retain private request/result ownership.
Subscription snapshots are not individually journaled; output inspection records an identity/omission receipt instead of duplicating its payload.
[Recording limits and failure rules](RECORDING_REFERENCE.md) still apply.

`record` defaults to true.
`record: false` omits an evaluation's call, result, and associated output from Markdown and QMD, while preserving journal/raw capture.
Inspection does not add entries to those human-readable documents.
This setting does not hide objects from later code or add data isolation.

Output attribution follows observed cell/post-cell intervals.
Background native output arriving after private completion can enter idle output and later MCP responses, or a later cell's interval.
Keep instrumentation synchronous or give background work separately owned streams when it must stay out of agent responses.

## Try the Shiny fixture

The [Shiny fixture](../tests/fixtures/control_shiny/README.md) demonstrates chat, shared transcripts, variable inspection, direct code submission, subscriptions, and immediate R/Python execution.
