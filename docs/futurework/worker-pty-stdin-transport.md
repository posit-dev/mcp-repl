# Worker PTY Stdin Transport

## Use Case

Some runtimes may need TTY-like stdin for their normal interactive
hooks. For example, a Python embedding that relies on
`PyOS_ReadlineFunctionPointer` may only use that hook when stdin is a
TTY. A future worker may therefore need to be launched with a PTY rather
than a plain pipe.

## Boundary

PTY selection is not part of the worker sideband protocol. A worker
cannot negotiate PTY use after it has already launched, because the
server must choose the stdin transport before spawning the process.

If needed, PTY use should be a pre-launch worker configuration value,
such as:

```text
stdin_transport = "pipe" | "pty"
```

This belongs in the server's worker registry or backend launch spec, not
in steady-state request handling.

## Constraints

- Server steady-state request handling should remain generic: write
  normalized input bytes to worker stdin, consume sideband facts, and
  deliver OS controls.
- PTY use must not reintroduce prompt parsing, prompt stripping, or
  interpreter-specific completion logic in the server.
- Raw stdout/stderr behavior may change under a PTY, including echo,
  CRLF conversion, stream merging, terminal width, and control
  sequences. Any PTY design must account for this explicitly.
- Interrupt behavior may become closer to terminal behavior, but pending
  input cleanup still needs tests because bytes already delivered to the
  runtime cannot always be recovered.
- A PTY design should revisit whether sideband interrupt
  acknowledgements are useful. The current protocol direction does not
  wait for an ack before the OS interrupt because that could deadlock if
  readline or runtime evaluation blocks the worker control path. A PTY
  implementation may change that risk profile, but recovery still needs
  to be proven by input accounting plus an unsatisfied readline boundary
  or session end.

## Acceptance Direction

Before making any built-in worker use a PTY, add Zod or equivalent
conformance coverage that compares pipe and PTY launch modes against the
same public server contract.
