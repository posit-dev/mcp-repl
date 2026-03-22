# Remove Pager Stage 1

## Summary

- Remove server-side paging, truncation notices, synthetic input summaries, and echo elision from `repl`.
- Keep worker behavior and the sideband wire protocol unchanged.
- Replace ring-based reply assembly with a single always-present `PendingOutputTape`.
- Preserve sideband ordering metadata so later stages can make richer echo decisions without changing visible behavior in this stage.

## Status

- State: completed
- Scope: response assembly and formatting changes only
- Last updated: 2026-03-21

## Decision Log

- Keep the worker behavior and sideband wire protocol unchanged so the stage remains isolated to server-side rendering.
- Make `PendingOutputTape` the shared accumulator owned by `WorkerManager`.
- Preserve raw UTF-8, stderr prefixes, image update collapsing, and sideband ordering metadata for later follow-on work.
- Return echoed input verbatim in this stage and defer smarter echo suppression to later work.
