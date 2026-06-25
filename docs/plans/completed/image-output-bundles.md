# Image Output Bundles

## Summary

- Shipped the mixed text/image output-bundle contract for files mode.
- Kept `transcript.txt` worker-only and used `events.log` as the ordered mixed-content index.
- Preserved full image history in the bundle while keeping only anchor images inline.

## Status

- State: completed
- Last updated: 2026-04-16
- Current phase: completed

## Delivered Slice

- Added bundle layout with `transcript.txt`, `events.log`, `images/`, and `images/history/`.
- Indexed worker text with `T` rows and image history with `I` rows in `events.log`.
- Preserved same-reply plot-update history in the bundle while keeping the visible reply bounded with first/last image anchors.

## Follow-On Work

- Per-turn history bundles remain future work in `docs/futurework/per-turn-history-bundles.md`.
- Timeline convergence is recorded in `docs/plans/completed/unified-output-timeline-pipeline.md`.

## Decision Log

- 2026-04-06: Landed the mixed-bundle files-mode contract as part of the oversized-output rollout.
- 2026-04-16: Archived this narrow sub-plan after the shipped contract was fully covered and subsumed by the completed oversized-output work.
