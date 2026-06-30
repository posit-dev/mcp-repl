# Interrupt Sideband Boundary

## Summary

- Keep interrupt behavior split across two independent mechanisms:
  - Sideband IPC may only discard already queued, unconsumed worker input.
  - The runtime always receives a real OS interrupt signal or Windows control event.
- Keep the server runtime-opaque. It sends `input_batch` for all non-empty input
  and does not branch on Python, R, cell execution, readline, or stdin mode.
- Replace prompt-free `ready` communication with `input_wait { prompt: null }`.
- Remove worker paths where sideband interrupt handling can inject, emulate,
  suppress, or observe runtime interruption.

## Status

- State: active
- Last updated: 2026-06-29
- Current phase: Unix and Windows implementation complete; unsupported
  non-Unix/non-Windows input-wait shape remains a fallback boundary.

## Motivating Scenario

- An MCP client sends Ctrl-C while Python or R user code is running, waiting in
  `input()`, waiting in readline, or sitting between top-level cells.
- The user should see runtime-native interrupt behavior:
  - Python signal handlers and `KeyboardInterrupt` come from Python observing the
    platform signal.
  - R interruption comes from R observing the platform interrupt state.
  - The sideband message must not become a second runtime interrupt channel.
- If Ctrl-C includes a tail, the tail should be delivered after a short
  best-effort settle window as a new `input_batch`.

## Current Direction

- Treat sideband cleanup as an ordered queue operation:
  - `discard_pending_input { discard_id }`
  - `discard_pending_input_ack { discard_id, discarded_input }`
- Because `input_batch` and `discard_pending_input` are delivered on the same
  worker IPC channel, the worker can deterministically discard only input that
  arrived before the discard message.
- Treat runtime interruption as platform behavior:
  - The server sends the OS interrupt every time.
  - The worker never converts sideband cleanup into `KeyboardInterrupt`,
    `PyErr_SetInterrupt`, R interrupt state, or backend-specific wake behavior.
- Model worker input waits as a real blocking receive:
  - Sending `input_batch` wakes the consumer by the queue/channel primitive.
  - Closing the queue for shutdown wakes the consumer by the same primitive.
  - Discarding queued input does not wake the runtime.
  - There is no busy polling loop for normal operation.

## Long-Term Direction

- The sideband protocol names should make misuse difficult:
  - Use `discard_pending_input`, not `interrupt`, for queue cleanup.
  - Use `input_wait { prompt: string | null }`, not a separate `ready`.
- Python and R may implement their input queues differently, but they expose the
  same worker contract:
  - emit prompt-visible waits with `prompt: string`;
  - emit prompt-free top-level waits with `prompt: null`;
  - consume all server input as `input_batch`;
  - receive runtime interrupts only through the OS.
- Tests should prove public behavior through MCP calls and worker protocol
  fixtures, not by asserting internal helper state.

## Protocol Shape

```yaml
server_to_worker:
  input_batch:
    input: string

  discard_pending_input:
    discard_id: u64

  shutdown: {}

worker_to_server:
  input_wait:
    prompt: string | null
    # string: runtime is waiting for stdin/readline and prompt should be shown
    # null: runtime is ready for a top-level input batch; suppress prompt/echo

  input_line:
    prompt: string | null
    text: string
    # null suppresses synthetic prompt/echo presentation

  discard_pending_input_ack:
    discard_id: u64
    discarded_input: bool

  session_end: {}
```

## Server Pseudocode

```python
def send_user_input(input: str) -> Reply:
    if input == "":
        return poll_or_drain_current_request()

    ctrl_c, tail = split_leading_interrupt(input)

    if ctrl_c:
        interrupt_runtime()

        if tail:
            sleep(INTERRUPT_TAIL_SETTLE_WINDOW)
            worker.send(input_batch(input=tail))
            return wait_for_reply_for_new_request()

        return wait_for_interrupt_reply_or_timeout()

    worker.send(input_batch(input=input))
    return wait_for_reply_for_new_request()
```

```python
def interrupt_runtime() -> None:
    discard_id = next_discard_id()

    worker.send(discard_pending_input(discard_id=discard_id))

    # Best effort only. This wait improves common ordering, but it is not part
    # of the runtime interrupt guarantee.
    wait_for_discard_ack(discard_id, timeout=SHORT_ACK_TIMEOUT)

    # This always happens, whether the ack arrives or times out.
    process.send_os_interrupt()
```

```python
def on_worker_event(event):
    match event:
        case input_wait(prompt=None):
            mark_worker_ready(prompt_visible=False)
            suppress_prompt_and_echo_for_next_top_level_batch()

        case input_wait(prompt=str() as prompt):
            mark_worker_waiting_for_stdin(prompt=prompt)
            show_prompt_if_reply_is_finalized_here(prompt)

        case input_line(prompt=None, text=text):
            append_worker_text(text)
            suppress_synthetic_prompt_echo()

        case input_line(prompt=str() as prompt, text=text):
            append_synthetic_prompt_echo(prompt, text)

        case discard_pending_input_ack(discard_id=id, discarded_input=flag):
            complete_best_effort_ack_wait(id, flag)

        case session_end():
            finalize_session_end()
```

Server invariants:

- Do not check whether the worker is Python or R.
- Do not check whether the worker will consume input as a cell or as readline.
- Do not send backend-specific interrupt commands.
- Do not skip the OS interrupt because sideband cleanup succeeded.
- Do not rely on ack completion for correctness; it only improves ordering in
  common cases.

## Worker Pseudocode

```python
def sideband_loop():
    for message in sideband.recv():
        match message:
            case input_batch(input=input):
                input_queue.send(input)
                # The send operation wakes the blocking consumer.

            case discard_pending_input(discard_id=id):
                discarded = input_queue.discard_already_queued_items()
                sideband.send(discard_pending_input_ack(
                    discard_id=id,
                    discarded_input=discarded,
                ))
                # No runtime interrupt state is touched here.
                # No runtime wait is explicitly woken here.

            case shutdown():
                input_queue.close()
                return
```

```python
def input_queue_send(input: str):
    lock(queue)
    queue.push(input)
    condvar.notify_one()
    unlock(queue)


def input_queue_recv_blocking() -> str | Closed:
    lock(queue)
    while queue.empty() and not queue.closed:
        condvar.wait(queue)

    if queue.closed:
        return Closed

    return queue.pop_front()


def discard_already_queued_items() -> bool:
    lock(queue)
    discarded = not queue.empty()
    queue.clear()
    unlock(queue)
    return discarded
```

Worker invariants:

- `discard_pending_input` only removes queue entries that were already received.
- `discard_pending_input` must not set runtime interrupt flags.
- `discard_pending_input` must not call Python signal APIs, R interrupt APIs, or
  platform signal APIs.
- `discard_pending_input` must not affect future `input_batch` messages.
- Input arrival and queue close are the only queue operations that wake a
  blocking input consumer.

## Python Pseudocode

```python
def python_cell_loop():
    while session_running:
        sideband.send(input_wait(prompt=None))

        cell = input_queue_recv_blocking()
        if cell is Closed:
            break

        # If a SIGINT landed before the tail batch arrived, Python observes it
        # before running the new cell.
        PyErr_CheckSignals()

        execute_python_cell(cell)
```

```python
def python_readline(prompt: str) -> str | Interrupted:
    sideband.send(input_wait(prompt=prompt))

    # This is the managed equivalent of Python waiting for console input. It
    # blocks until input arrives, shutdown closes the queue, or the OS interrupt
    # path causes Python to observe SIGINT.
    line = input_queue_recv_blocking_with_python_signal_receptivity()

    PyErr_CheckSignals()

    if line is Closed:
        raise EOFError

    sideband.send(input_line(prompt=prompt, text=line))
    return line
```

Python invariants:

- Cell execution waits in `python_cell_loop`.
- `input()`, `help()`, `pdb`, `sys.stdin`, and readline waits run through
  `python_readline`.
- Sideband cleanup has no Python runtime effect.
- `KeyboardInterrupt` and user signal handlers come from Python observing the
  platform signal.
- Normal waits must not busy poll. If a platform needs a special wake mechanism
  for signal receptivity, it belongs in the Python readline/input wait
  abstraction, not in sideband cleanup.

## R Pseudocode

```python
def r_console_read(prompt: str) -> str | Interrupted:
    sideband.send(input_wait(prompt=prompt))

    line = input_queue_recv_blocking_with_r_interrupt_receptivity()

    R_CheckUserInterrupt()

    sideband.send(input_line(prompt=prompt, text=line))
    return line
```

R invariants:

- Sideband cleanup only discards queued input.
- R interruption remains the result of R observing the platform interrupt state.
- Worker protocol behavior stays aligned with Python from the server's point of
  view.

## Phase Status

- Phase 0: complete - land this plan and use it as the review boundary.
- Phase 1: complete - update protocol names and docs to `discard_pending_input`
  and nullable `input_wait`.
- Phase 2: complete - update server interrupt flow so OS interrupt is always
  sent after best-effort sideband cleanup.
- Phase 3: complete - update worker queue handling so sideband cleanup cannot
  wake or mutate runtime interrupt state. Python and R sideband cleanup now
  discard queued input only.
- Phase 4: complete on Unix and Windows - update Python and R waits to preserve
  runtime-native interrupt behavior without normal-operation busy polling.
  Unix Python uses a Python signal wake fd; Unix R uses a runtime input wake
  pipe plus a SIGINT bridge that marks R's pending interrupt flag and lets the
  main R thread call `R_CheckUserInterrupt()`. Windows Python and R use runtime
  input events plus console-control event wake handlers; the handlers wake the
  runtime wait and return control to the runtime's normal interrupt handling.
- Phase 5: complete on Unix and protocol fixtures - update public tests and
  protocol fixture tests to lock the boundary. Windows runtime tests still need
  to be run on a Windows host.

## Locked Decisions

- The server always sends non-empty user input as `input_batch`.
- The server never needs to know whether input will be consumed as a cell,
  readline, stdin, or another runtime-specific mode.
- Sideband cleanup is not a runtime interrupt path.
- A successful cleanup ack never justifies skipping the OS interrupt.
- Prompt-free readiness is represented as `input_wait { prompt: null }`.
- The Ctrl-C tail settle delay is best effort. It improves common user-visible
  behavior but is not a correctness guarantee.

## Open Questions

- Whether unsupported non-Unix/non-Windows platforms should keep the current
  condvar/timed fallback or fail fast instead of pretending to satisfy the full
  interrupt contract.

## Next Safe Slice

- Push the branch and run the Windows tests on a Windows host.
- Decide whether to fail fast on unsupported non-Unix/non-Windows platforms.

## Verification

- `env RUSTFLAGS=-Dwarnings cargo check`
- `env RUSTFLAGS=-Dwarnings cargo build`
- `python3 tests/run_integration_tests.py --binary target/debug/mcp-repl`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `env RUSTFLAGS=-Dwarnings cargo test --quiet`
- `cargo +nightly fmt`
- `env RUSTFLAGS=-Dwarnings cargo build --release --locked`

## Stop Conditions

- Stop and update this plan if a fix requires the server to branch on Python vs
  R, or on cell vs readline consumption.
- Stop and update this plan if a worker-side sideband handler needs to call a
  runtime signal/interruption API.
- Stop and ask for a design decision if Python cannot observe OS interrupts
  from the managed readline/input wait without timed polling or a dedicated
  signal-receptive wait abstraction.

## Decision Log

- 2026-06-29: Chose a strict split between ordered sideband queue cleanup and
  runtime-native OS interruption to prevent architectural leakage from review
  fixes.
- 2026-06-29: Chose `input_wait { prompt: null }` instead of `ready` so the
  worker has one readiness event with explicit prompt/echo presentation
  semantics.
- 2026-06-29: Chose backend-opaque server input dispatch. The worker owns where
  each `input_batch` is consumed.
- 2026-06-29: Renamed the protocol to `discard_pending_input` and removed the
  separate `ready` worker event in favor of `input_wait { prompt: null }`.
- 2026-06-29: Split Python sideband cleanup from runtime input wakeups; discard
  ack handling now clears queued input without waking the runtime wait.
- 2026-06-29: Replaced Unix R's timed `ReadConsole` wait with a blocking input
  wake pipe. A Unix SIGINT handler marks R's pending interrupt flag and writes
  the pipe so the main R thread observes the interrupt through
  `R_CheckUserInterrupt()`.
- 2026-06-29: Chose parser-layer rejection for stale generic `interrupt`,
  `interrupt_ack`, and `ready` protocol messages so review cannot treat old
  interrupt sideband names as compatibility surfaces.
- 2026-06-29: Added Windows runtime input wake events for Python and R. Queue
  send/close sets the queue event; real console-control delivery sets the signal
  event; sideband discard still only clears queued input and sends an ack.
