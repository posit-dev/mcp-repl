# Worker PTY Stdin Transport

Status: implemented for Unix built-in Python and custom worker launch
configuration. This note is retained as historical design context; the current
contract is documented in `docs/architecture.md`,
`docs/worker_sideband_protocol.md`, and `docs/output_timeline.md`.

## Use Case

Some runtimes may need TTY-like stdin for their normal interactive
hooks. For example, a Python embedding that relies on
`PyOS_ReadlineFunctionPointer` may only use that hook when stdin is a
TTY. A worker may still use a PTY-backed launch path for terminal behavior, but
managed `repl` input is queued through IPC and then served by worker-owned
runtime stdin hooks or bridges.

## Boundary

PTY selection is not part of the worker sideband protocol. A worker
cannot negotiate PTY use after it has already launched, because the
server must choose the stdin transport before spawning the process.

PTY use is a pre-launch worker configuration value, such as:

```text
stdin_transport = "pipe" | "pty"
```

This belongs in the server's worker registry or backend launch spec, not in
steady-state request handling.

## Constraints

- Server steady-state request handling remains generic: send fresh `input_batch`
  messages over IPC, consume sideband facts, and deliver OS controls.
- PTY use must not reintroduce prompt parsing, prompt stripping, or
  interpreter-specific completion logic in the server.
- Raw stdout/stderr behavior may change under a PTY, including echo,
  CRLF conversion, stream merging, terminal width, and control
  sequences. The current docs and tests account for this explicitly.
- Interrupt behavior may become closer to terminal behavior, but pending
  input cleanup still needs tests because bytes already delivered to the
  runtime cannot always be recovered.
- A PTY design should revisit whether sideband interrupt
  acknowledgements are useful. The current protocol direction does not
  wait for an ack before the OS interrupt because that could deadlock if
  readline or runtime evaluation blocks the worker control path. A PTY
  implementation may change that risk profile, but recovery still needs
  to be proven by worker-owned queue accounting plus an unsatisfied runtime
  input boundary or session end.

## Acceptance Result

The repository now has custom worker coverage for PTY launch with sideband IPC
kept separate from visible PTY output, plus public Python backend tests for the
managed stdin queue and bridge surfaces.
