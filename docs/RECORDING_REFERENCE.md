# Recording reference

Console retains calls, notes, source, input, requirements, output, and artifacts on the execution host, without redaction.
For reading a recording, retrieving omitted output, or making a report, see [Recordings and reports](RECORDING.md).

## Storage and retention

If `.agents/console` already exists in the launch directory, recordings use `.agents/console/sessions/<run-id>/`.
Otherwise they use `~/.agents/console/sessions/<run-id>/`; `MCP_CONSOLE_HOME` replaces that fallback Console directory.
Console does not create a project `.agents` directory just to select project storage.
There is no aggregate quota or automatic cleanup, so remove old recordings deliberately.

## Recording activation

The first received MCP `tools/call` targeting `send`, or a request on the optional [integrator control channel](INTEGRATOR_CONTROL.md), activates a recording for the Console process.
Notes, polls, input, controls, rejected submissions, and requests cancelled before execution all activate recording.
Initialization, tool discovery, pings, startup output, and startup failure alone do not create a persistent recording.

Console retains preceding protocol traffic and startup text/images temporarily.
The activating request publishes that prelude in its observed order with its original timestamps.
If the process exits without either kind of request, Console discards the temporary capture.
The recording retains the process start time separately from publication time.
Restarts and automatic worker replacement create numbered sessions within that same recording.
Named sessions share this recording; each worker lifetime receives its own number, independently of the public `session` name.
Worker-start events retain the name so concurrent sessions and their replacements can be distinguished.
Each cell gets a log file when execution starts, even if it produces no text output.

| Path within the recording                                                           | Contents                                                                  |
| ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| `capture/events.jsonl`                                                              | Protocol, lifecycle, and capture events.                                  |
| `capture/sessions/NNN/startup.txt`                                                  | Text observed before the first cell.                                      |
| `capture/sessions/NNN/cell-CCC.txt`                                                 | Text observed while cell `CCC` executes.                                  |
| `capture/sessions/NNN/cell-CCC-post.txt`                                            | Text observed after cell `CCC` and before the next cell.                  |
| `capture/sessions/NNN/startup-III.png`, `cell-CCC-III.png`, `cell-CCC-post-III.png` | Numbered images in the corresponding interval; non-PNG images use `.bin`. |
| `sessions/session-NNN.qmd`                                                          | Notes and source for worker lifetime `NNN`.                               |
| `transcript.md`                                                                     | Readable calls, results, and links to retained output.                    |

## Readable transcript and session documents

`transcript.md` records calls, notes, source, results, polls, input, controls, and worker session boundaries, with links to source documents and retained output.
Notes appear as prose before any source in the same call; the journal retains the original `note` string in that call's arguments.
Its Console process metadata and the journal's `recording_started` event retain the executable's version, including the Git revision for development builds when available.
It preserves response previews and omission notices while leaving routine protocol and lifecycle details in the journal.
Results describe response assembly; local transport delivery is recorded separately in `capture/events.jsonl`.

Each `sessions/session-NNN.qmd` contains notes and source associated with that actual worker lifetime, including failed and rejected source, with error-permitting chunk options.
Ordinary notes retain their Markdown and HTML.
A note containing recognized metadata delimiters, executable code syntax, or Quarto shortcodes is wrapped as a whole in a `{verbatim}` chunk so it remains an example rather than changing document settings or executing code.
Documents with notes select the Quarto engine from recorded source cells, so examples in notes do not change it: `knitr` for R, `jupyter` for Python or SQL without R, and `markdown` without source.
This fallback covers common notebook annotations; it does not sanitize arbitrary Markdown or HTML, and Quarto may still preprocess shortcodes or inline syntax inside verbatim examples.
The journal and Markdown transcript retain the original note text.

### Source association

Source admitted before worker acquisition stays pending until its worker is known, even if an initial response has already returned.
A validation-rejected restart leaves its source with the surviving addressed worker.
A note submitted with source follows that source's worker association, including across a restart.
Without source, a note belongs to the selected logical session's worker when the call is received, or waits for that session's next actual worker if none exists.
Notes can create a QMD for a worker that receives no source.
QMD entries follow original call order, with each note before any source from the same call, even when source association is delayed.
A failed acquisition does not create a session, and a session without source or notes need not create a QMD.
Source and notes without a worker association remain in the journal and Markdown transcript.

### Requirements and omitted operations

QMD metadata records the owning session's accepted R and Python declarations, Python version and cutoff constraints, and DuckDB extension additions.
Retained R requirements carry into replacement sessions; rejected additions do not change metadata, and replacement declarations do not rewrite the retiring session's document.
The original working directory is descriptive metadata rather than an execution root.
QMDs omit results, images, input, polls, and control wrappers.
Existing recordings are not migrated or rewritten.

An integrator evaluation can set `record: false` to omit its source, result, and associated output from the Markdown and QMD projections.
Inspection requests on that channel also stay out of those documents.
Their journal events and raw capture remain retained under the same limits and failure behavior.
Immediate integrator images use `capture/sessions/NNN/call-CCC-III.png`, where `CCC` is the call ID and `III` is the image number within that request.
Their image-event owner identifies the session and call, without a cell or interval; each request has its own image retention budget using the limits below.

## Output intervals

Each actual worker session numbers executed cells from `001`.
Notes, polls, input, controls, and rejected source do not create cells.
Source remains in the journal and documents even when it does not execute.

Output belongs to the interval in which Console observes it:

- `startup`: before the first cell begins, including the entire lifetime of a worker that executes no cells.
- `cell-NNN`: while that cell executes.
- `cell-NNN-post`: after the cell ends and before the next begins.

The final interval includes shutdown output.
A later poll can return output from several intervals or sessions without changing its ownership.
Console does not infer which computation caused background output or impose chronology across independent streams.

Every executed cell has a text file, including silent cells.
Startup and post-cell text files are created only when text arrives.
One file retains each interval's raw text, up to 1 GiB, even when text and images alternate.
Each retained image has its own numbered file in that interval; PNGs use `.png`, and other emitted image formats use `.bin`.
Raw images have a separate budget per startup, cell, or post-cell interval, and per immediate integrator request: 64 MiB of decoded image data, 4,096 files, and 64 KiB of MIME metadata.
An image is saved whole only if it fits the remaining raw budget.
The response preview has its own limits: images omitted from a preview can still be saved, and images that exceed the raw budget can still appear in a response.
Console retains emitted text and images, not arbitrary files written by submitted code.
Output observed before worker acquisition or after worker termination is retained in process-output events rather than assigned to a worker session.
Omission notices for that output reference the capture journal.

## Capture events and completion

`capture/events.jsonl` records one JSON object per event, including MCP and integrator requests and responses, MCP notifications, canonical calls/results, worker lifetimes, dependency selections, and references to output logs and images.
MCP protocol events reference the canonical send call/result records instead of duplicating their payloads.
Events preserve observation timestamps and sequence numbers and identify the session, cell, and interval explicitly.
Text events give byte ranges in the interval log so text/image ordering can be reconstructed.
Each retained image has an `image_retained` event with its owner, artifact ID, and file path relative to the recording directory.
An `image_omitted` event records an image rejected by the raw retention budget; it has no file to locate.
For an image returned without raw retention, the result record has `retained: false` and no file reference; it does not duplicate the image data in the journal.
Result records associate fragments of the assembled response with their observed intervals, preserving previews, prompts, errors, omission notices, and image references.
A response assembly event and a completed local transport write are separate observations.
Neither proves that the client acknowledged the response or that a model read it.

Worker events record actual start, readiness, and termination.
The `cell_started` event associates a call with its executed cell; accepted source that is withheld before execution remains in `tool_call` without a cell event or log.
A restart request does not itself establish a session; a failed preparation or launch that never acquires a worker has no session number.
Interrupting a surviving worker stays in its session.
The journal can help inspect a recording with scripts or investigate a failed call.
Its format may change between Console releases.

### File completion

Completed managed files become read-only while retaining their existing privacy permissions.
Cell logs close at their cell boundary; startup and post-cell logs stay writable until their next boundary.
Images become read-only after writing.
The journal, Markdown transcript, and session QMDs remain writable until worker output, queued capture, source associations, result writers, and transport settlement can no longer update them.
Directories remain writable while Console needs them.
These permissions protect against accidental edits; abrupt termination can leave files writable.

Returned paths refer to the Console host.
Project paths are normally relative to the launch directory; fallback paths are absolute.
A long-location notice may instead name a path relative to the selected Console directory.

### Recording failures {#recording-failures}

Recording can fail or fall behind while computation continues.
A notice identifies files or artifacts that are unavailable or only partially retained; do not assume all output was saved.

A background writer performs all recording I/O, so recording files can trail the replies that describe them.
Replies do not wait for the writer, except when an omission notice names retained files: the reply waits up to 5 seconds for the writer to reach them.
If the writer is still behind, the notice gives the same path and adds `recording writer stalled; retained files may be missing or incomplete`.
If the writer is still behind on the recording's first write, there is no directory yet, so the notice gives no path and adds `recording writer stalled; retained files are not yet published` instead; later notices give the path once the writer resumes.
The files are completed if the writer resumes.
While the backlog remains, later replies with such notices carry the same warning without waiting again.
If recording has stopped, the notice gives the same path and adds `recording stopped; retained files may be missing or incomplete` because output still queued for the writer when it stopped was discarded.
Integrator previews and preview subscriptions do not wait for the writer.
Their omission notices include these warnings only if recording has stopped or a backlog that a reply's bounded wait gave up on remains.
Before the writer publishes the recording, their notices omit the path and say `retained files are not yet published`; a preview subscription sends a new snapshot with the path when the recording is published.

A failed write to an output file is reported by the first reply that waits for the writer and still holds output from that file.
A reply that shows the file's output in full does not report the failure.
In both cases, the file's `cell_output` or `session_output` event records the retained and discarded byte counts.

Creating an output file is limited to 5 seconds.
A creation that takes longer is reported as a file that was not created.
Until the creation returns, later output files are not created either, and it can leave an empty file behind.

When the backlog exceeds 16 MiB or 512 operations, output text and images wait for the writer, slowing a worker that produces them faster than they can be recorded to the writer's pace.
Console also waits before reading the next MCP message or integrator request when 64 operations, or 16 MiB of operations, without output text or images are queued, slowing a burst of requests to the writer's pace.
Queued output text and images do not count toward that bound, so a worker flooding output does not delay the request that interrupts it.
If the writer completes no operation for 5 seconds while output, images, or the next request wait for it, or if its backlog reaches 1,024 operations or 64 MiB excluding queued output text and images, Console disables recording, reports the failure on stderr, and continues to receive MCP traffic.
The writer prints the report after its current filesystem call returns.
Shutdown still waits for the writer without a limit ([RT-6](TODO.md)).

The recording journal and generated file formats are not independently versioned APIs.
Implementation and failure-handling details belong in [Architecture](ARCHITECTURE.md#output-and-delivery).
