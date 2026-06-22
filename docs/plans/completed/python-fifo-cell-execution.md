# Python FIFO Cell Execution

## Summary

- Rework built-in Python cell execution around one worker-owned inbound FIFO.
- Keep embedded CPython, AST cell execution, `sys.displayhook`, managed stdin, and plot capture.
- Remove server-side dependence on Python prompt semantics for routing.

## Status

- State: completed for this slice
- Last updated: 2026-06-21
- Current phase: follow-up cleanup complete

## Current Direction

- The Python worker owns one queue of client payloads.
- The top-level cell loop is the default consumer and treats the next payload as a complete cell.
- A running Python read callback temporarily owns the same queue and consumes the next payload as stdin bytes.
- The server should only send input and wait for worker readiness/completion; it should not classify Python prompts as top-level, readline, help, pdb, or raw stdin.

## Long-Term Direction

- Unix and Windows stdin adapters should call the same queue-consumer primitive.
- Prompt text is only a visible runtime prompt while Python is actually waiting for input.
- Top-level readiness should not render a Python idle prompt.

## Phase Status

- Phase 0: completed - inspected current branch and useful existing pieces.
- Phase 1: completed - public regressions now assert FIFO cell/stdin ownership and prompt-free top-level readiness.
- Phase 2: completed - full required checks passed.

## Locked Decisions

- Keep embedded CPython.
- Do not add Jupyter/IPython or a separate Python worker process.
- Do not add MCP tool modes or arguments for cell versus stdin input.
- Route each non-empty payload once, at the top of the call. If Python is waiting for stdin, the whole payload is stdin; otherwise the whole payload is one cell.

## Open Questions

- Background Python thread reads still share the same exclusive read-consumer primitive; fairness between competing foreground/background reads remains a known limitation and is not broadened in this slice.
- Empty-prompt reads keep the existing generic wait-status behavior rather than inventing a visible prompt.

## Follow-Up Notes

- Revisit only future failures that point to this prompt-free Python cell contract or session-end respawn behavior.

## Stop Conditions

- Stop if the design requires a separate Python process, a Jupyter/IPython dependency, or server-side Python source parsing.
- Stop if fixing a regression requires reintroducing overlapping semantic prompt state machines.

## Decision Log

- 2026-06-21: Use a checked-in plan because the change spans Python worker state, platform stdin adapters, server protocol completion, and public behavior.
- 2026-06-21: Prefer a worker readiness signal without rendered prompt for top-level idle completion; real read prompts remain prompt-bearing waits.
- 2026-06-21: Treat startup readiness as protocol-driven: initial `input_wait` preserves prompt-bearing readiness, while `ready` permits prompt-free startup without a backend-specific Python spawn case.
- 2026-06-21: Convert Python `SystemExit` inside the embedded cell runner into a worker exit request, then emit `session_end` before CPython finalization. Session-end respawn detaches old output/sideband readers so finalization or inherited holders cannot block the tool response.
- 2026-06-21: Required checks passed: `cargo check`, `cargo build`, `python3 tests/run_integration_tests.py --binary target/debug/mcp-repl`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --quiet`, and `cargo +nightly fmt`.
