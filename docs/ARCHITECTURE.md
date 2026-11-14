# Architecture

The server owns session state that survives worker replacement.
Each worker owns its interpreters, objects, and SQL connections.
The native runner enforces OS permissions and owns its process-tree cleanup contract.

This page describes the boundaries contributors must preserve.
For observable behavior, use the [public interface](API.md), [language reference](RUNTIME_REFERENCE.md), and [sandbox permissions](SANDBOX_CONFIGURATION.md).

## Process layout

| Path                   | Processes                                                  |
| ---------------------- | ---------------------------------------------------------- |
| Dependency preparation | Server → native runner → resolver → package tools          |
| Code execution         | Server → sandbox frontend → native runner → relay → worker |

All processes run on the execution host.
Remote placement belongs to the client or deployment tooling.
`--no-sandbox` removes native enforcement and cleanup, while retaining server/worker separation.

The frontend verifies the pinned companion before launching it.
On Unix it becomes the runner through `exec`; on Windows it waits for the runner.
The relay owns only the direct worker.
It must not compete with the runner for native supervision, managed temporary storage, or descendant cleanup.
Windows preparation uses the pinned MXC companion and per-operation Jobs for resolver retirement.

## Ownership

| Owner             | Responsibility                                                                                                 |
| ----------------- | -------------------------------------------------------------------------------------------------------------- |
| Server            | MCP admission, logical sessions, generations, accepted requirements, response delivery, and recording.         |
| Preparation owner | Runtime discovery and materialization, including cleanup before publishing results.                            |
| Native runner     | Native policy, managed temporary storage, and descendant retirement.                                           |
| Relay             | Worker descriptors, stream forwarding, interrupts, and direct-child reaping.                                   |
| Worker            | Interpreter initialization, evaluation, managed input, semantic output, SQL state, and environment activation. |

The [server-relay](RELAY_PROTOCOL.md) and [relay-worker](WORKER_PROTOCOL.md) interfaces are private JSONL protocols.
Interactive stdin and native stdout/stderr are separate streams.
Each stream preserves its own order; observation order across streams does not establish a global chronology.

### Integrator control

The optional Unix [control channel](INTEGRATOR_CONTROL.md) uses the same registry, permissions, and lifecycle owners as MCP.
MCP connection closure owns shutdown; control clients cannot keep the server alive or grant more permissions.

An ordinary control evaluation uses the same admission and preparation boundaries as an MCP cell, but retains its own result owner through socket disconnection.
MCP cannot claim that result, and pre-existing MCP output stays with MCP.
Inspection and subscriptions copy bounded views without consuming output, acknowledging delivery, or blocking producers.

Immediate execution has a separate result owner.
R runs on its owning thread at an event boundary; Python runs on a background thread.
Neither path takes the ordinary cell's managed output.
Native process streams cannot identify their writing thread and retain foreground attribution.
Preparation admission remains held until the immediate request completes.
Restart or failure settles that request against its original worker connection.

## Startup and runtime ownership

The server captures configuration once.
New logical sessions use that captured configuration and connection authority, with independent startup, preparation, requirements, and worker owners.
The default session prelaunches in the background; named sessions enter the registry on their first code call or explicit control `create`.
MCP initialization, discovery, and pings do not wait for runtimes.

The connection's capacity pool reserves a slot for the default session.
Startup, preparation, and replacement retain their session's slot independently of request cancellation.
Stopping a named worker preserves its registry entry and accepted requirements.
Capacity cannot be released while either preparation or execution still owns resources.

With `inherit_codex`, the first `send` captures client authority in transport request order before any preparation or execution process starts.
Cancellation cannot let a later request choose the policy.
An untranslatable permission rejects the whole client profile; fallback permissions cannot widen a selected profile.
The captured policy survives retries and worker replacement.
See the [sandbox adapter](../src/server/sandbox.rs) for translation rules.

Transport readiness connects services before interpreter bootstrap.
Early code can reserve the ordinary evaluation slot, but there is no cell queue and an admitted cell is never replayed.
Cancelling a wait does not cancel shared startup.
Changed requirements wait for built-in R bootstrap before replacing an unused worker, because native R initialization cannot safely be retried in place.

Initial discovery or preparation failure permits an explicit restart, or a later code call to a named session, only after all relevant owners confirm cleanup.
Once configured startup or native R initialization may have run, another attempt requires explicit restart.
A later successful stage cannot erase earlier missing cleanup evidence.

### Worker

One coordinator serializes ordinary interpreter initialization and cells on one thread.
R affinity, CPython thread-state/GIL ownership, and reentrant cross-language callbacks constrain this design.
Signal handlers mark state and wake waits; they do not enter interpreters.
Do not hold locks or mutable state borrows across reentrant calls.

R and Python are peer runtimes.
Console owns CPython bootstrap and services; reticulate provides compatibility and conversion.
Bridge attachment must use the running interpreter.
Late R initialization can mutate the process environment while native threads exist; this remains [open work](TODO.md#runtime-and-resource-limits).

SQL connections and retained results stay in their owning runtime.
Actual R capability selects the default managed provider independently of language visibility or initialization order.
Attaching a runtime must not replace a user-selected connection.
Providers retain bounded preview rows and one native result, fetch on demand, and invalidate before cleanup or connection replacement.
Driver buffering and conversion details belong in [the SQL implementations](../src/sql/), including the managed DuckDB Arrow adapter in [bridge.R](../src/sql/bridge.R).
The [language reference](RUNTIME_REFERENCE.md#retrieve-the-last-sql-result) owns consumption and lifetime rules.

## Generations and operations

Evaluations, input, controls, resolver callbacks, and candidates retain the generation that admitted them.
Old work must never reach a replacement.
Automatic recovery can reuse a generation, so connection identity also fences follow-up work and activation receipts.

A cell has one admission owner before a worker exists.
It records pending, dispatched, or withheld source separately from worker entry.
Terminal worker receipts or generation failure settle execution; observation deadlines and output delivery do not.
An unclaimed result retains the ordinary cell slot.
The [send reference](API.md#operations) owns combined control-and-cell ordering.

Restart prepares changed requirements before retiring the worker.
Failure before acceptance preserves the previous declaration and worker; failure after retirement cannot restore live state.
An established default-session worker can recover once automatically without replaying code or input.
Named workers wait for another code or nonempty-input call after cleanup.
Interrupt selection retains its original resolver, startup, or worker target through acknowledgment; it cannot retarget a later operation.

## Preparation and activation

The resolver materializes environments under the separate [preparation policy](PACKAGE_CONFIGURATION.md).
It publishes results only after process and I/O cleanup.
The server owns accepted declarations; materialization alone does not accept one.

A live candidate becomes accepted only after the worker validates and activates it and the server receives the matching current-generation receipt.
Accepted activation survives a later import or cell error.
Stale or unaccepted candidates do not.
Explicit changes and runtime callbacks share environment-change ownership so an older resolution cannot overwrite a newer declaration.

Mixed-language changes can have separate acceptance points.
Package installation, cache writes, and site hooks are not transactional.
A stopped named session can accept prestart preparation without rewriting its retired worker's recording.
Unresolved activation failures still require explicit restart.

## Retirement and cancellation

Each launch has one retained retirement operation shared by restart, EOF, startup failure, and recovery.
Later requests observe it without renewing deadlines, resending controls to old PIDs, or joining the same tasks independently.

Retirement closes admission, requests shutdown, bounds command/output handling, settles owned I/O, and obtains the owning runner's cleanup evidence.
Blocked writes, inherited descriptors, or partial frames must not prevent retirement.
Aborting a partial command ends that transport; a control cannot be inserted into its bytes.

Physical cleanup, protocol settlement, and output delivery are distinct outcomes.
Available output is drained within bounds even if cleanup fails.
Graceful worker shutdown runs R finalizers and device cleanup; only runner-managed storage has an additional temporary-storage retirement guarantee.

The recovery barrier requires direct-launcher reaping and settled I/O.
Runner loss can still allow replacement without confirming descendant or private-storage cleanup.
Pipe EOF, forced launcher termination, and a suppressed diagnostic do not prove native cleanup.
These [lifetime limits](SANDBOX_CONFIGURATION.md#supported-hosts-and-lifetime-limits) remain part of the supported boundary.

Connection closure stops registry admission and retires every session's startup, preparation, and execution resources under one shutdown deadline.
It also bounds MCP delivery and drains already-received transport input before settling responses.
Recording finalization has a separate filesystem constraint described below.

## Output and delivery

Each session owns an ordered output tape and finite response cuts.
The server applies text/image budgets and notices.
One recoverable response stays owned until local delivery or cancellation settles it.
Failed delivery returns the complete claimed region to its session; aggregate polls settle or restore every session's claim together.
Polling cannot continue after its call is cancelled.

Cancelled delivery can race with bytes already visible to the client, so recovery does not guarantee exactly-once observation.
A journaled result proves response assembly; a successful local write still does not prove client acknowledgment.

### Recording ownership and failures

One connection-wide journal records protocol traffic, calls, results, and actual worker lifetimes.
Logical names, generation IDs, and recording worker numbers have different purposes.
Output, images, notes, source associations, and requirement metadata retain their original worker owner through replacement and delayed publication.
A source admission is not evidence that the worker entered the cell.

The bounded capture queue carries every recording operation, including output-file bytes and images, to one writer thread.
Worker dispatch, retirement, handlers, and replies queue operations and do not perform recording I/O.
Four waits on the writer remain, each bounded at 5 seconds: creating an output file, pacing a producer of output text or images behind a large backlog, pacing request intake behind queued request operations, and assembling a reply whose omission notice names retained files.
Output pacing blocks the producer's thread, which can hold the session's output state, and intake pacing delays every later request, including cancellation, so both continue only while the writer makes progress.
Markdown and QMD are projections; neither is an interpreter checkpoint.
Reported capture failures disable recording without stopping computation; projection failures can leave the journal and raw files usable.
The [recording reference](RECORDING_REFERENCE.md#recording-failures) owns retention limits, file layout, and the visible effects of a slow writer.

Finalization waits for the writer after worker retirement and transport settlement.
The integrator's `recording` request also waits for the writer so that its reply covers the request.
Neither wait is bounded: an indefinitely blocked filesystem operation can keep the process alive at shutdown because fail-open error handling cannot cancel that OS operation ([RT-6](TODO.md)).
Local capture algorithms and identity rules belong in [transcript.rs](../src/transcript.rs) and its modules.

## Policy extensions and compatibility

[policy_extensions.sbpl](../src/sandbox/policy_extensions.sbpl) owns Console's macOS allowances and their evidence.
Removing an allowance requires a regression through the actual sandboxed workload and selected companion version.
A host resolver success is not worker-sandbox evidence.
Keep rule-specific rationale beside the rule; track unresolved review in [TODO](TODO.md#sandbox-and-platforms).

## Where to look in source

| Concern                             | Entry points                                                                                                                                |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| MCP admission and presentation      | [server/arguments.rs](../src/server/arguments.rs), [server/presentation.rs](../src/server/presentation.rs)                                  |
| Configuration capture               | [config/](../src/config/), [settings/](../src/settings/)                                                                                    |
| Session startup and generations     | [server/startup.rs](../src/server/startup.rs), [server/sessions.rs](../src/server/sessions.rs), [worker_client.rs](../src/worker_client.rs) |
| Evaluation and integrator ownership | [worker_client/evaluation.rs](../src/worker_client/evaluation.rs), [worker_client/integrator.rs](../src/worker_client/integrator.rs)        |
| Interpreter coordination            | [worker/coordinator.rs](../src/worker/coordinator.rs), [python.rs](../src/python.rs), [sql.rs](../src/sql.rs)                               |
| R graphics publication              | [r_graphics.rs](../src/r_graphics.rs), [r_graphics.c](../src/r_graphics.c)                                                                  |
| Preparation and activation          | [resolver/preparation.rs](../src/resolver/preparation.rs), [worker_client/environment/](../src/worker_client/environment/)                  |
| Native launch and relay             | [sandbox.rs](../src/sandbox.rs), [worker_client/process.rs](../src/worker_client/process.rs), [worker_relay.rs](../src/worker_relay.rs)     |
| Recording and projections           | [transcript.rs](../src/transcript.rs), [transcript/](../src/transcript/)                                                                    |

Follow these owners into their modules.
[Boundary tests](../tests/boundaries/README.md) provide executable evidence.
Recognized user environment variables belong in the [environment reference](ENVIRONMENT.md); private transport and activation markers are not configuration or proof of enforcement.

## Developer terms

| Term                         | Meaning                                                                                                              |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| Logical session              | A named registry entry with retained configuration, requirements, and worker ownership.                              |
| Generation                   | The admission and callback identity fencing lifecycle operations; automatic recovery can replace a worker within it. |
| Sideband                     | Private JSONL commands and semantic events, separate from interactive stdin and native streams.                      |
| Candidate / activation       | A proposed environment and the step making it usable by the worker; server acceptance is separate.                   |
| Retirement / cleanup receipt | Stopping an owned resource and confirming its owner's cleanup contract, beyond stream closure.                       |
| Output cut                   | A finite response boundary in observed output.                                                                       |
| Execution host               | The environment where Console and its files and processes run.                                                       |
