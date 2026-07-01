`repl_reset` restarts the active R REPL session.

Python servers do not advertise this tool; use `\u0004` in `repl` input for a plain Python reset.

Behavior:
- Clears in-memory session state (objects, variables, loaded runtime state tied to the process).
- Starts a fresh worker session and returns the new-session status output.
- `repl_reset` does not delete server-owned output artifacts, but already-disclosed bundle paths remain valid only until quota pruning or server exit.
- Prefer this when the intent is explicit lifecycle control or memory cleanup after large one-off work.

Arguments:
- none
