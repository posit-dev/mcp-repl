# Unified Output Timeline Pipeline

## Status

Completed.

## Result

The server now uses one canonical `OutputTimeline` for normal replies, files
overflow, and pager overflow. The timeline stores raw stdout/stderr bytes,
worker IPC text, sideband markers, images, server notices, request boundaries,
and session-end markers. Files mode and pager mode consume the same timeline
and keep only their policy-specific behavior.

Generated input echoes are projection behavior:

- normal replies do not show generated echoes
- pager pages show consumed-input echoes as transcript context
- bundle transcripts include consumed-input echoes
- captured output is never parsed to infer or suppress echoes

The detailed current contract lives in `docs/output_timeline.md`.
