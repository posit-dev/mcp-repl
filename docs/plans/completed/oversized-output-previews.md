# Oversized Output Previews

## Summary

- Shipped files-mode oversized-output bundles for text-only and mixed text/image replies.
- Kept polling as the primary interaction model and kept worker transcripts server-owned.
- Left later architecture follow-ons to separate futurework docs instead of keeping this rollout active.

## Status

- State: completed
- Last updated: 2026-04-16
- Current phase: completed

## Delivered Slice

- Replaced the default files-mode pager-era large-output behavior with bounded inline previews plus server-owned output bundles.
- Added hidden timeout bundle creation, lazy disclosure, retention/cleanup, and follow-up poll backfill for `transcript.txt`.
- Shipped mixed text/image bundles with `events.log`, top-level final image aliases, preserved `images/history/`, and first/last inline anchor images.
- Added public coverage for text spill, timeout backfill, detached idle output, mixed bundles, and image-history behavior.

## Locked Decisions

- Polling remains the public interaction model.
- Worker transcript files stay server-owned and contain worker-originated REPL text only.
- Server-only status lines stay inline-only; mixed bundles may record server omission notices in `events.log`.
- Files mode discloses bundle paths in normal reply text instead of relying on a default transcript-read tool.

## Follow-On Work

- Per-turn history bundles remain future work in `docs/futurework/per-turn-history-bundles.md`.
- Converging pager/files mode onto one shared resolved timeline remains future work in `docs/futurework/unified-output-timeline-pipeline.md`.
- Interaction polish remains future work in `docs/futurework/repl-interaction-rough-edges.md`.

## Decision Log

- 2026-03-21: Chose server-owned bundle paths over a default transcript-read tool so the retrieval flow stayed simple.
- 2026-04-06: Shipped bundle-backed files mode, mixed text/image bundles, and backend-specific tool-description updates.
- 2026-04-16: Closed the rollout plan after PR #27 landed and moved remaining follow-ons to futurework.
