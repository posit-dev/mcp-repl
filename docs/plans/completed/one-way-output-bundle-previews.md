# One-Way Output Bundle Previews

## Summary

- Move files-mode output-bundle previews onto a one-way memory-to-disk model.
- Keep the public bundle layout unchanged: `transcript.txt`, `events.log`, `images/`, and `images/history/` remain the server-owned on-disk history surface.
- Stop building later replies by rereading previously written bundle files from disk.
- Keep only the bounded preview state needed for the next reply in memory while spilling the full history to files.

## Status

- State: completed
- Last updated: 2026-04-17
- Current phase: completed

## Current Direction

- Treat disk as append-only bundle history, not as the source of truth for visible reply previews.
- Refactor `ActiveOutputBundle` so it caches the bounded preview state needed for later polls:
  - first preview image
  - last preview image
  - bounded head text
  - bounded tail text
  - flags and counters needed to render the existing disclosure notice
- Start with image preview state first, because the current reply path still rereads old image files from the bundle directory when rebuilding later responses.
- Follow with text-preview caching so later small polls do not depend on reconstructing preview state from spilled files.

## Long-Term Direction

- The end-state contract is one-way: once content is spilled to the bundle directory, `mcp-repl` does not need to read it back to answer later polls.
- Disk remains the durable history surface for clients and debugging; memory owns the bounded preview shown inline in normal replies.
- Missing or externally modified bundle files should not matter to visible reply construction after the server has already retained the needed preview state in memory.
- Directory recreation for continued appends may still be reasonable when parents disappear, but visible reply generation should not depend on successful rereads of old bundle files.

## Phase Status

- Phase 0: completed
  - Define the one-way contract and the bounded in-memory preview state.
- Phase 1: completed
  - Add cached image-preview state to `ActiveOutputBundle` and remove old-image rereads from later reply rendering.
- Phase 2: completed
  - Confirm that later text replies already render from in-memory retained reply items and do not reread `transcript.txt`.
- Phase 3: completed
  - Replace reread-oriented regressions with one-way contract coverage for deleted bundle image and transcript files.
- Phase 4: completed
  - Run full validation and close the initiative.

## Locked Decisions

- Do not change the public files-mode bundle layout or path-disclosure contract as part of this refactor.
- Do not add tamper-detection logic for previously written bundle files.
- Do not rely on rereading previously written image files to build later visible replies.
- Keep preview state bounded in memory; do not retain the full spilled history in RAM.
- Preserve the current polling model and the existing distinction between inline preview content and the disclosed bundle path.

## Open Questions

- None for this slice.

## Next Safe Slice

- None. The planned work is complete.

## Stop Conditions

- Stop if the slice requires retaining unbounded image or text history in memory.
- Stop if preserving the current visible preview contract requires a broader redesign of reply materialization than this plan assumes.
- Stop if the work starts changing the public bundle file format, tool descriptions, or polling contract.
- Stop if directory-recreation behavior grows into speculative recovery logic for states the public API does not need to support.

## Decision Log

- 2026-04-17: Decided that files-mode output bundles should be one-way from memory to disk. Disk is the server-owned history surface; memory owns the bounded preview needed for later replies.
- 2026-04-17: Scoped the first implementation slice to image preview caching, because the current image bundle reply path still rereads previously written files from disk.
- 2026-04-17: Kept the public bundle layout unchanged for this initiative so the refactor can land without redefining the client-facing files-mode contract.
- 2026-04-17: Began the image-preview implementation by caching the first-history and latest image previews on `ActiveOutputBundle` and moving the public regression toward “later replies do not depend on old bundle image files remaining on disk”.
- 2026-04-17: Completed the image-preview slice. Later image-bundle replies now render from cached preview images in memory instead of rereading old image files from the bundle directory.
- 2026-04-17: Confirmed the text spill path already satisfied the one-way contract for visible replies. Later text replies render from in-memory retained reply items, and a new public regression now covers transcript deletion and recreation without replaying previously spilled text.
