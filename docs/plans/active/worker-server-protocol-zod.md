# Worker-Server Protocol Contract and Zod Conformance Worker


## Status

Draft blocking contract for the embedded Python work.

This document describes the target server/worker boundary. It is
intentionally stricter than the current implementation. The current
implementation still has server-side backend branches, prompt
reconciliation, and interpreter-state inference that this contract is
meant to remove.

## Motivating Scenario

A third party wants to add a Julia worker without changing server
request handling. They can provide an executable that speaks the worker
protocol. The server launches it, writes user input bytes to stdin,
forwards OS interrupts, receives ordered output and structural facts,
and finalizes MCP replies without knowing whether the runtime is R,
Python, Julia, Zod, or something else.

The same contract must be testable without a real interpreter. Zod is
the conformance worker: a small dummy worker that implements the full
protocol with deterministic echo behavior.

## Boundary Rule

The server is an MCP session manager and response finalizer. The worker
is the runtime owner.

During steady-state request handling, the server must not:

- Branch on interpreter or backend identity.
- Parse stdout or stderr to identify prompts.
- Strip prompt-looking text from output.
- Fabricate interpreter prompts.
- Interpret input syntax, prompt text, or continuation state.
- Maintain a shadow state machine for primary, continuation, or
  client-input prompts.
- Decide whether an interrupt should reach the runtime based on prompt
  text or inferred interpreter state.
- Depend on R-, Python-, Julia-, or Zod-specific completion rules.

During steady-state request handling, the worker must:

- Make stdin work for its runtime.
- Own prompt detection and prompt text.
- Own runtime-specific output capture.
- Report stdin accounting facts.
- Report when the runtime is waiting for more stdin.
- Arrange OS interrupt/reset/shutdown delivery into the runtime.
- Report session termination.

Interpreter semantics are data emitted by the worker, not behavior
inferred by the server.

## Server Responsibilities

The server owns:

- MCP tool surface: `repl` and `repl_reset`.
- Worker process lifecycle: launch, restart, shutdown, crash detection.
- Sandbox policy selection and process placement.
- One active write to worker stdin at a time.
- Tool-call timeout policy and polling of an already-active turn.
- Writing `repl()` input bytes to worker stdin, adding one trailing
  `\n` when a non-empty input does not already end in `\n`.
- Sending server-to-worker interrupt notifications for worker input
  bookkeeping.
- Delivering OS-level interrupt/reset/shutdown controls to an existing
  worker or process group.
- Capturing unowned raw stdout/stderr as best-effort visible output.
- Combining worker-originated output, server notices, and images into
  MCP replies.
- Oversized-output file and pager presentation.
- Bundle retention and cleanup.

The server may render generic server notices, such as timeout, worker
crash, or "waiting for stdin with an empty prompt". These notices must
be derived from explicit worker events, not from prompt parsing or
backend-specific inference.

## Worker Responsibilities

A conforming worker owns:

- The runtime process or embedded interpreter.
- Runtime stdin placement and any internal stdin buffering.
- Runtime prompt callbacks or equivalent prompt observation.
- Runtime output capture for worker-owned stdout/stderr.
- Runtime image or rich-output capture.
- A sideband control listener that can receive interrupt notifications
  without being blocked by runtime evaluation.
- Runtime response to OS interrupt/reset/shutdown controls.
- Session termination reporting.

A worker may be written in any language. It does not need to link to
Rust code. It only needs to implement stdin placement, the sideband
event schemas, and the ordering rules below.

## Worker Launch Configuration

The server must expose a way to launch a worker from an arbitrary
executable plus arguments. This is required for Zod conformance tests
and for third-party workers such as Julia. This does not need to be a
polished user-facing API in the first pass. It may be treated as an
internal implementation boundary and left undocumented for end users,
but it must be firm enough that the server can launch a worker without
request-handling code knowing the worker language.

An acceptable first shape is an explicit worker specification file, such
as TOML or JSON, passed to the CLI instead of `--interpreter`. That file
can be verbose and unergonomic if it names every launch detail directly.
Future work can add a friendlier user-facing CLI or registry layer on
top of the same boundary.

A launch specification should include at least:

- executable path;
- argument list;
- working directory policy;
- environment variables or environment overlay;
- stdin transport selection, initially `pipe`;
- sideband endpoint bootstrap;
- sandbox policy to apply before or during launch.

The launch specification is pre-start configuration. It is not
negotiated in the worker handshake, because the server must choose the
executable, arguments, stdio handles, sideband endpoint, and sandbox
placement before the worker can send `worker_ready`.

The target model is that the server owns sandbox selection and
enforcement: it sets up the sandbox, launches the worker inside it, and
captures the worker's stdin, stdout, stderr, and sideband endpoint. This
keeps third-party workers from needing to implement the server's sandbox
policy. If a platform-specific sandbox cannot technically wrap an
arbitrary external worker, that limitation should be treated as an open
design question for the launch layer, not as a reason for steady-state
request handling to branch on worker identity.

Open sandbox questions:

- Can each supported sandbox mode wrap an arbitrary worker executable
  with the same guarantees as built-in R and Python workers?
- Does any platform require a small server-owned launcher process
  between the sandbox mechanism and the worker executable?
- Which environment variables, temp directories, writable roots, and
  inherited handles are server-owned and therefore injected into every
  worker?
- How should launch validation fail when a custom worker cannot be
  placed in the requested sandbox?

## Transport

There are two channels:

- worker stdin: server-to-worker user input bytes.
- sideband: bidirectional UTF-8 JSON Lines, one JSON object per line.

The server receives the MCP `repl()` argument as decoded JSON text and writes
that string's UTF-8 bytes to worker stdin. For non-empty input, if the final
byte is not `\n`, the server appends exactly one `\n` byte before writing. This
makes a tool-call payload a complete line-oriented stdin submission without
requiring the worker to know where the MCP request ended. The server does not
otherwise frame the bytes, count logical lines, or reserve special byte
sequences for protocol messages.

The server may inspect only the first byte of `repl()` input for a leading
control prefix. A leading `\u0003` requests interrupt. A leading `\u0004`
requests reset. The server consumes at most one leading control byte. The
remaining bytes, if any, are a normal stdin payload after the control completes.
Control bytes anywhere after the first byte are ordinary stdin data.

The worker owns runtime stdin placement and any internal buffering. The
worker does not need to know MCP request boundaries to terminate pending
line reads; the server has already completed the input with a newline
when needed.

The sideband stream carries worker-owned output, structural facts, and
server-to-worker control notifications. The current sideband bootstrap
can remain:

- Unix: inherited file descriptor named by an environment variable.
- Windows: named pipe path named by an environment variable.

Worker stdout and stderr are runtime implementation details. Raw
stdout/stderr capture remains a fallback for output the worker does not
own, such as direct file descriptor writes by child processes. Raw
stdout/stderr is not a control protocol.

Server-to-worker sideband messages are not user-input transport. They do
not carry `repl()` payload bytes, and the worker must not treat them as
stdin.

Sideband message schemas are strict. Unknown required fields, unknown
message types, invalid enum values, or invalid base64 are protocol
errors.

## Text and Byte Encoding

Sideband itself is UTF-8 JSONL. Prompt text uses a JSON string:

- `readline_start.prompt`

Stdin accounting uses base64 byte payloads:

- `readline_input_bytes.data_b64`
- `readline_discard_bytes.data_b64`

The server decodes those fields and compares the bytes with the
active-turn stdin byte queue.

This does not add a new user-visible input restriction beyond MCP. A
normal `repl()` call supplies a JSON string inside a UTF-8 JSON-RPC
message, so invalid input byte sequences are not representable by the
public tool call. The server writes the UTF-8 bytes of that decoded
string to worker stdin. Sending exact arbitrary non-UTF-8 bytes to stdin
is out of scope for this contract and would require an explicit future
binary input surface.

Runtime output is different. Output hooks can surface byte payloads that
are not guaranteed to be valid UTF-8. For example, R's console callback
provides a `const char *` plus a length, and Python exposes raw
`sys.stdout.buffer` / `sys.stderr.buffer` writes. Therefore
`output_text.data_b64` is byte-preserving and the worker must not be
required to decode arbitrary output bytes as text.

The server owns MCP-facing output normalization. It decodes
byte-preserving worker output and raw stdout/stderr capture into valid
MCP text only when rendering a reply or bundle. That normalization must
not feed back into prompt detection, completion, interrupt routing, or
stdin accounting.

## Version Handshake

The first worker-to-server message must be:

```json
{
  "type": "worker_ready",
  "protocol": { "name": "mcp-repl-worker", "version": 2 },
  "worker": { "name": "zod", "version": "0.1.0" },
  "capabilities": {
    "images": false
  }
}
```

The server must reject an unsupported `protocol.name` or
`protocol.version` before sending user input. `protocol.version` is a
single integer, not semver: there is no major/minor fallback behavior.
Capability fields describe protocol features. They must not cause
interpreter-specific steady-state code paths.

The `worker.name` field is descriptive. It is useful for logs and
diagnostics, but server request handling must not switch on it.

Handshake field notes:

- `capabilities.images`: whether the worker emits `output_image` events.

Raw stdout/stderr capture is not a negotiated capability. The server
will always capture raw worker stdout/stderr as unowned fallback output,
and the worker cannot opt out. Raw output capture still must not drive
prompt detection, completion, or interrupt routing.

## Turn Model

The server sends user input by writing the normalized `repl()` bytes to worker
stdin. Normalization means appending one trailing `\n` to non-empty input that
does not already end in `\n`. The worker reports which input bytes have crossed
the worker/runtime input boundary and when the runtime asks for more input.

The server keeps one active turn after it writes to worker stdin. A turn
is complete when the worker emits a `readline_start` event that cannot
be satisfied by bytes the server already wrote for that active turn.
This means the runtime has reached a line-read boundary, no prior
active-turn bytes remain to satisfy it, and the runtime is ready for the
next client input. The prompt text for that boundary comes from the
worker event.

The server must not infer completion from raw stdout/stderr, prompt
text, language syntax, output quiescence, byte counts supplied before
the write, or backend identity.

A `readline_start` with an empty prompt is valid:

```json
{ "type": "readline_start", "prompt": "" }
```

This means the worker observed a runtime line-read boundary, and the
runtime supplied no prompt text for that boundary. The server must still
treat the event as real. If all previously written input has been
accounted for, the active turn is complete even though there is no
prompt text to display.

If the MCP reply needs visible text, the server may render a generic
server-owned status such as `<<repl status: waiting for input>>`. It
must not invent a runtime prompt such as `> ` or `>>> `, because that
would falsely attribute prompt text to the worker/runtime.

## Input Events

### `readline_start`

Worker to server:

```json
{
  "type": "readline_start",
  "prompt": ">>> "
}
```

Fields:

- `prompt`: prompt text supplied by the runtime. This field is required,
  and the empty string is valid.

The worker emits this when the runtime enters a line-read operation and
supplies prompt text for that read. The read may or may not actually
block: it can be satisfied immediately by bytes already available on
stdin.

If the server still has bytes from the active turn that have not been
matched by `readline_input_bytes` or `readline_discard_bytes`, this
prompt is satisfied by already-written input and the turn is not
complete. If no such bytes remain, this prompt is unsatisfied and the
server may seal the reply for the active turn.

For an unsatisfied `readline_start`, the server will render non-empty
worker-supplied prompt text in the MCP response to show that the runtime
is ready for the next input. This rendering is response presentation
derived from worker data; it is not prompt fabrication, stdout parsing,
or prompt-state inference. The server must not inspect the prompt text
to decide whether it is a primary prompt, continuation prompt, `input()`
prompt, or something else.

Prompt rendering from `readline_start` is not `output_text`. A
conforming worker should not also emit the same prompt as stdout/stderr
unless the runtime actually produced visible output that the worker
cannot suppress. If prompt-like text does arrive as output, the server
must preserve it as ordinary output and must not deduplicate it by
comparing it with `readline_start.prompt`.

### `readline_input_bytes`

Worker to server:

```json
{
  "type": "readline_input_bytes",
  "data_b64": "MSsxCg=="
}
```

Fields:

- `data_b64`: exact active-turn bytes delivered to the runtime-facing
  input layer, encoded as base64.

The server may use `readline_input_bytes` only for generic accounting
against the bytes it already wrote to worker stdin. It must not
interpret the bytes as language syntax. Invalid base64 or a mismatch
with the server's active-turn byte queue is a protocol error because it
means the worker's input placement is not describing what it delivered
to the runtime-facing input layer.

`readline_input_bytes` is not itself a completion signal. Completion is
the next unsatisfied `readline_start` or `session_end`.

### `readline_discard_bytes`

Worker to server:

```json
{
  "type": "readline_discard_bytes",
  "data_b64": "Y2FuY2VsbGVkCg=="
}
```

Fields:

- `data_b64`: exact active-turn bytes that the worker discarded without
  delivering to the runtime, encoded as base64.

The worker emits this only for bytes it can account for. The server
removes these bytes from the active-turn byte queue exactly like
delivered input bytes, but it does not display them as runtime output.
Invalid base64 or a mismatch with the server's active-turn byte queue is
a protocol error.

If the worker discards input after interrupt or reset cleanup and cannot
report which bytes were discarded, the server cannot prove recovery for
any control tail. In that case, the worker should not emit
`readline_discard_bytes` for unknown bytes, and the server must not
write a tail that depends on clean recovery.

## Output Events

Worker-owned output must be sent over sideband. This gives the server an
ordered timeline without parsing raw pipes.

### `output_text`

Worker to server:

```json
{
  "type": "output_text",
  "stream": "stdout",
  "data_b64": "aGVsbG8K"
}
```

Fields:

- `stream`: `stdout` or `stderr`.
- `data_b64`: raw output bytes.

Prompt-looking bytes are ordinary output unless the worker reports them
in `readline_start.prompt`. The server must preserve prompt-looking
output exactly.

### `output_image`

Worker to server:

```json
{
  "type": "output_image",
  "image_id": "plot-1",
  "mime_type": "image/png",
  "data_b64": "...",
  "update": false
}
```

Fields:

- `image_id`: worker-local stable id for update grouping.
- `mime_type`: MIME type.
- `data_b64`: image bytes.
- `update`: true when this updates a previous image with the same
  `image_id`.

The server owns MCP response image IDs. The worker owns only source
identity and update semantics.

## Server Control Messages

Server-to-worker sideband control messages are for worker bookkeeping.
They are not a substitute for OS controls and do not carry user input.

### `interrupt`

Server to worker:

```json
{ "type": "interrupt" }
```

The server sends this message when it is about to deliver an OS
interrupt to the existing worker process or process group. It applies to
the current worker session and the current active turn, if any. It
carries no request id because the server allows only one active turn.

The worker uses this message to clean up worker-owned input state. In
response, the worker should cancel or drain any pending stdin bytes that
it owns or can observe, and emit `readline_discard_bytes` for the exact
active-turn bytes it discarded. The worker must not emit discard events
for bytes it already delivered to the runtime-facing input layer, bytes
it cannot identify, or bytes that belong to no active turn.

The worker's sideband control listener must not be blocked by runtime
evaluation. If the worker cannot process the sideband `interrupt` before
the runtime consumes pending bytes, those bytes should be reported as
`readline_input_bytes`, not `readline_discard_bytes`.

The server does not wait for an acknowledgement to `interrupt`. Recovery
is proven only by later worker events: exact input accounting followed
by an unsatisfied `readline_start`, or `session_end`.

## OS Controls

Interrupt, reset, and shutdown are not stdin protocol bytes. The runtime
effect is OS-level: the server applies the platform interrupt, reset, or
termination mechanism to an existing worker process or process group.

Interrupt has an additional sideband notification for worker
bookkeeping. That notification lets the worker discard pending input it
owns or can observe. The sideband notification is not an opt-in
interrupt mechanism and does not replace the OS interrupt.

The only model-facing control syntax in `repl()` input is the one-byte
leading prefix. The server must not scan the rest of the payload, split
around later control bytes, or interpret language syntax.

The server must not inspect prompt text or backend type before
delivering OS controls. If a worker exists, the server delivers the
control. If no worker exists, the server must not spawn a worker solely
to interrupt or shut down nothing.

Workers cannot opt out of OS controls. Graceful shutdown is a server-owned
stdin close followed by a bounded wait; OS escalation is common across all
workers and remains server-owned.

For reset and shutdown, if the server considers the worker busy, it
sends an interrupt first. This is not configurable by the worker. After
the interrupt, the server waits for the normal bounded server-owned
grace period for the worker to return to an unsatisfied `readline_start`
or end the session. The server then closes worker stdin and waits for
natural exit. If the worker does not exit within the server-owned graceful
shutdown timeout, the server escalates to OS termination. The server must not
write interpreter shutdown code to stdin and must not use sideband shutdown
commands to deliver graceful shutdown.

### Interrupt

Interrupt maps to the platform's normal interrupt mechanism for the
worker: for example, SIGINT or the Windows process/control equivalent.
The worker is responsible for arranging the runtime so that this control
interrupts active evaluation or pending runtime input.

The server attempts a bounded write and flush of the sideband
`interrupt` notification before delivering the OS interrupt. Failure or
timeout while sending the sideband message must not suppress or
materially delay the OS interrupt.

If the server has accepted a new stdin payload but not yet written all
bytes to worker stdin, interrupt cancels the unwritten tail. Bytes
already accepted by the OS or runtime cannot be recalled by the server.
The sideband `interrupt` notification tells the worker to make a
best-effort attempt to discard pending input that it owns or can
observe. The protocol does not require recovery of bytes already
delivered into runtime buffers.

Recovery after interrupt is a server-observed protocol state, not a
sleep or a signal-delivery acknowledgement. The worker has recovered
only when it emits one of these events after the interrupt:

- an unsatisfied `readline_start` after the active-turn byte queue has
  been fully accounted for by input and/or discard byte events, meaning
  the runtime is ready for the next client input and no bytes from the
  interrupted turn remain to satisfy that read;
- `session_end`, meaning the old runtime is gone and cannot consume a
  follow-up tail.

If neither event arrives before the server-owned interrupt recovery
timeout, the worker has not recovered for protocol purposes. If bytes
remain in the active-turn queue and the worker cannot account for them
as delivered or discarded, the worker has not recovered for protocol
purposes even if it emits another `readline_start`.

After interrupt, normal event rules continue. The worker may emit output
and eventually an unsatisfied `readline_start`, or it may emit
`session_end` if the runtime terminates.

If a leading interrupt prefix has a non-empty tail, the server writes
the tail only after recovery via an unsatisfied `readline_start`. If the
worker emits `session_end` or does not recover before the server-owned
timeout, the tail is not written.

### Reset

Reset means end the current worker session and start a fresh one for the
next tool call.

If the worker is busy, the server interrupts first. If the worker is
then known to be waiting at an unsatisfied `readline_start`, the server closes
worker stdin and waits for natural exit. If the worker does not exit within
the server-owned timeout, the server escalates to OS termination.

If a leading reset prefix has a non-empty tail, the server writes the
tail only after reset has completed and the replacement worker has
emitted an unsatisfied `readline_start`.

### Shutdown

Shutdown follows the same interrupt-then-graceful-stdin-then-OS
escalation shape as reset, except no replacement worker is started.

## Session Events

### `session_end`

Worker to server:

```json
{
  "type": "session_end",
  "reason": "shutdown"
}
```

Fields:

- `reason`: why this worker session ended.
- `message_b64`: optional diagnostic bytes.

Reason values:

- `shutdown`: the server intentionally ended the worker session and will
  not start a replacement for this session.
- `reset`: the server intentionally ended the worker session so it can
  start a fresh replacement session.
- `runtime_exit`: the runtime exited through normal user/runtime action,
  such as R `quit()`, Python `sys.exit()`, or a Zod `exit` command.
- `crash`: the worker process, runtime, or worker-owned child needed for
  the session ended unexpectedly or abnormally.
- `protocol_error`: the worker is ending because it detected an
  unrecoverable server/worker protocol violation.

After `session_end`, the worker must not emit output events.

## Ordering Rules

The sideband stream is the authoritative order for worker-owned events.

For a conforming worker:

1. `worker_ready` is first.
2. `readline_start` is emitted when the runtime enters a line-read
   operation, before it reads input bytes for that operation.
3. `readline_input_bytes` is emitted after the worker delivers input
   bytes to the runtime-facing input layer.
4. `readline_discard_bytes` is emitted after the worker discards
   accounted-for input bytes during interrupt/reset cleanup.
5. `output_text` and `output_image` are emitted in runtime-visible
   order.
6. `session_end` is final.

Raw stdout/stderr capture is not a control protocol. The server may
include raw captured bytes as unowned visible output, but raw bytes must
not drive completion, prompt detection, echo suppression, or interrupt
routing.

Server-to-worker `interrupt` messages are ordered on the
server-to-worker sideband stream. Worker-to-server recovery facts are
ordered on the worker-to-server sideband stream. The server must not
assume that writing the `interrupt` message means the worker has already
processed it; later `readline_input_bytes`, `readline_discard_bytes`,
`readline_start`, and `session_end` events determine recovery.
Built-in Unix Python currently has a private `python_interrupt` /
`python_interrupt_ack` cleanup handshake so it can drain PTY input before
SIGINT; that acknowledgement is transitional and not part of the generic
worker protocol.

## Timeout and Polling

If an MCP tool call times out before the worker emits an unsatisfied
`readline_start` or `session_end`, the server returns a server-owned
timeout notice and keeps the active turn.

An empty poll while a turn is active does not become runtime input. It
waits for or drains events for the active turn. If the worker later
emits an unsatisfied `readline_start`, the server uses that explicit
event to finalize the poll reply.

A later non-empty `repl()` call after an unsatisfied `readline_start` is
written to worker stdin after the same trailing-newline rule: if the input is
non-empty and does not end in `\n`, append one `\n`. The server does not
otherwise canonicalize supplied stdin bytes such as `\r\n` or bare `\r`.
The worker/runtime decides whether those bytes answer an
interpreter-level prompt, continue an incomplete expression, or start a
new top-level evaluation.

A `repl()` call that begins with `\u0003` or `\u0004` first performs the
corresponding control operation. Any remaining tail bytes are written only under
the safe policy described in OS Controls, then receive the same trailing-newline
normalization as ordinary input.

## Error Handling

Protocol errors are fail-fast:

- Invalid JSON.
- Unknown sideband message type.
- Missing required field.
- Invalid enum value.
- Invalid base64.
- `readline_input_bytes.data_b64` that is invalid base64 or does not
  match bytes the server wrote for the active turn.
- `readline_discard_bytes.data_b64` that is invalid base64 or does not
  match bytes the server wrote for the active turn.
- Worker-owned output after `session_end`.
- Second non-empty input while a turn is still active.

The server should surface protocol errors as server-owned diagnostics
and end the worker session.

Runtime errors are not protocol errors. Workers should emit runtime
error text as `output_text` on `stderr`. The next unsatisfied
`readline_start` or `session_end` still drives reply finalization.

## What a Third-Party Worker Must Implement

A third-party worker must:

1. Start and connect the sideband JSONL stream from the environment.
2. Emit `worker_ready`.
3. Listen for server-to-worker sideband control messages without
   blocking on runtime evaluation.
4. Arrange for server-written input bytes to reach the runtime.
5. Emit `readline_start` when the runtime enters a line-read operation,
   before it reads input bytes for that operation.
6. Emit `readline_input_bytes` after delivering input bytes to the
   runtime-facing input layer.
7. Emit `readline_discard_bytes` for accounted-for active-turn bytes
   discarded during interrupt/reset cleanup.
8. Emit worker-owned output as `output_text` or `output_image`.
9. Arrange OS interrupt/reset/shutdown controls to affect the runtime.
10. Emit `session_end` before clean shutdown.
11. Avoid using raw stdout/stderr for worker-owned output.

If those requirements are met, the server must not need
language-specific code for the worker.

## Zod Worker

Zod is a dummy conformance worker. It is not an interpreter. It exists
to prove that the server is generic.

Zod must be an additional standalone binary, not another mode of the
production `mcp-repl` binary. It should be built only for development
and tests, for example as a separate workspace crate or other detached
test helper codebase. It must run as a separate child process and speak
the public worker protocol over stdin and sideband, exactly like a
third-party worker would.

Zod should not link to server or built-in worker internals, call private
Rust APIs, or depend on shared in-process state. Its value as a
conformance worker comes from being outside the implementation under
test. Tests may know how to locate and launch the Zod binary, but server
request handling must interact with it only through the same worker
launch configuration, stdin, OS controls, raw stdout/stderr capture, and
sideband protocol used for any other worker.

Zod should implement the same worker protocol as real workers and expose
a small deterministic command language through stdin:

- Any ordinary input echoes to stdout and then emits an unsatisfied
  prompt.
- `stderr <text>` emits stderr output and then emits an unsatisfied
  prompt.
- `wait <prompt>` changes the next unsatisfied prompt to the supplied
  bytes.
- `sleep <millis>` delays the next unsatisfied prompt so timeout and
  poll behavior can be tested.
- `interruptible <millis>` delays until either the timer finishes or a
  sideband or OS interrupt arrives.
- `interrupt-report <millis>` delays while recording sideband and OS
  interrupt delivery as separate output facts.
- `slow-shutdown <millis>` delays a later `exit` or EOF `session_end`
  so reset and shutdown graceful-exit timing can be tested.
- `hang-shutdown` accepts a later `exit` or EOF but never emits
  `session_end`, so reset and shutdown OS escalation can be tested.
- `image` emits a tiny deterministic PNG if Zod advertises image
  support.
- `exit` emits `session_end`.

Zod must not require server code to know that it is Zod during request
handling. The only Zod-specific code should be worker selection in test
setup.

## Zod Conformance Tests

The first protocol PR should add public tests that run through the MCP
surface with Zod as the worker:

- A simple input write echoes output and returns a worker-supplied
  prompt.
- Prompt-shaped stdout is preserved exactly.
- Empty prompts are reported from worker events, not fabricated by the
  server.
- Waiting-for-input state comes only from unsatisfied `readline_start`.
- A follow-up input after waiting state is written to worker stdin with
  the same trailing-newline normalization.
- Unterminated client input completes in line-oriented workers because
  the server appends one trailing newline before writing to worker
  stdin.
- A timed-out turn remains active and a later poll observes the
  unsatisfied `readline_start`.
- Non-empty input while a turn is active does not reach worker stdin.
- Ctrl-C sends the sideband `interrupt` notification and is delivered as
  an OS interrupt to an existing worker.
- Ctrl-C cancels any not-yet-written stdin tail, and the worker
  best-effort discards pending input it owns with `readline_discard_bytes`
  accounting.
- Interrupt tail input is sent only after all prior active-turn bytes
  are accounted for as delivered or discarded and the worker emits an
  unsatisfied `readline_start`.
- Interrupt completion is driven by worker events, not prompt parsing.
- Worker-owned stderr preserves ordering relative to stdout.
- Raw stdout/stderr never affects prompt or completion state.
- Reset interrupts first when the worker is busy, then closes worker stdin and
  escalates to OS termination if needed.
- Shutdown follows the same interrupt-then-stdin-close-then-OS rule and cannot
  be opted out of.
- Protocol errors fail fast and do not leave the server in a shadow
  interpreter state.

These tests should fail if server request handling branches on
interpreter identity or parses prompt-looking text.

## Migration Implications for R and Python

The current mixed protocol should be replaced rather than patched
indefinitely.

Required migration work:

- Add a worker launch configuration surface that can select a standalone
  Zod binary and, later, arbitrary third-party worker executables plus
  arguments.
- Keep worker stdin as the only user-input transport from server to
  worker.
- Remove `stdin_write`, `stdin_write_complete`, byte counts, line
  counts, `stdin_write_ack`, and the private Python interrupt
  acknowledgement.
- Remove IPC-carried request ids and request payloads.
- Replace server-inferred completion from prompt parsing with
  unsatisfied worker-emitted `readline_start`.
- Treat prompts as structured worker-owned data on `readline_start`, not
  text to discover or strip from stdout/stderr.
- Remove backend-conditioned prompt reconciliation from server request
  paths.
- Remove server continuation/primary/client-input prompt state.
- Keep raw stdout/stderr capture only as unowned output fallback.
- Make R, Python, and Zod pass the same server conformance tests.

After this migration, adding Julia or another language should be a
worker implementation task, not a server request-handling task.

## Current Refactor Status

- 2026-06-15: Active v3 turn id, idle completion, protocol-error
  latching, and session-end latching moved out of `src/ipc.rs` into
  `src/turn_state.rs`. IPC still owns legacy active-stdin accounting and
  readline stable-wait completion.
- 2026-06-15: Legacy stdin/readline completion state moved out of
  `ServerIpcInbox` into `src/legacy_request_state.rs`. IPC still owns
  legacy acknowledgement queues and protocol-version branching.
- 2026-06-15: Legacy `stdin_write_ack` and Python interrupt ack queues moved
  out of the general IPC message queue into `src/legacy_ack_state.rs`. IPC
  still owns protocol-version branching.
- Next safe slice: map the remaining IPC protocol-version branches, then delete
  the smallest compatibility branch that is already covered by public R,
  Python, and Zod behavior.

## Acceptance Criteria Before Unblocking Embedded Python

- This protocol contract is merged.
- Zod worker exists and passes the conformance tests.
- The server can launch Zod through the same custom worker
  executable/argument surface intended for third-party workers.
- Server steady-state request handling is worker-protocol driven, not
  backend-driven.
- User input travels to the worker only as stdin bytes, with exactly one
  trailing `\n` appended by the server when non-empty input does not already
  end in `\n`.
- R and Python workers emit `readline_start`/`readline_input_bytes` facts
  sufficient for the server to identify unsatisfied input waits.
- R and Python workers emit `readline_discard_bytes` for any active-turn
  input bytes they discard during interrupt/reset cleanup.
- The server does not parse or strip prompts from stdout/stderr.
- The server delivers OS interrupts to an existing worker without
  consulting interpreter state.
- Reset and shutdown interrupt first when busy, may use the handshake's
  exact graceful stdin command only when the worker is waiting for
  input, and always retain non-optional OS escalation.
- Existing public R and Python MCP behavior is preserved where it is
  part of the product contract.
