# Python FIFO Cell Execution

## Summary

- Rework built-in Python cell execution around one worker-owned inbound FIFO.
- Keep embedded CPython, AST cell execution, `sys.displayhook`, managed stdin, and plot capture.
- Remove server-side dependence on Python prompt semantics for routing.

## Status

- State: active
- Last updated: 2026-06-21
- Current phase: implementation

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
- Phase 1: active - add public regressions and collapse Python request/stdin ownership.
- Phase 2: pending - run full required checks.

## Locked Decisions

- Keep embedded CPython.
- Do not add Jupyter/IPython or a separate Python worker process.
- Do not add MCP tool modes or arguments for cell versus stdin input.
- Do not support code plus buffered stdin in one payload as the primary contract.

## Open Questions

- Background Python thread reads may need a narrower v1 ownership rule if they compete for stdin with foreground code.
- Empty-prompt reads may surface only a generic wait status or no visible prompt; concrete behavior will follow focused tests.

## Next Safe Slice

- Adjust public tests to assert no rendered top-level idle prompt after completed Python cells.
- Add a focused queue-ownership test that proves a follow-up payload is stdin only while a running read owns the queue, and then source code again after that read completes.

## Stop Conditions

- Stop if the design requires a separate Python process, a Jupyter/IPython dependency, or server-side Python source parsing.
- Stop if fixing a regression requires reintroducing overlapping semantic prompt state machines.

## Decision Log

- 2026-06-21: Use a checked-in plan because the change spans Python worker state, platform stdin adapters, server protocol completion, and public behavior.
- 2026-06-21: Prefer a worker readiness signal without rendered prompt for top-level idle completion; real read prompts remain prompt-bearing waits.
