# One-Way Output Bundle Previews

## Summary

- Move files-mode output-bundle previews onto a one-way memory-to-disk model.
- Keep the public bundle layout unchanged: `transcript.txt`, `events.log`, `images/`, and `images/history/` remain the server-owned on-disk history surface.
- Stop building later replies by rereading previously written bundle files from disk.
- Keep only the bounded preview state needed for the next reply in memory while spilling the full history to files.

## Status

- State: active
- Last updated: 2026-04-17
- Current phase: planning

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

- Phase 0: active
  - Define the one-way contract and the bounded in-memory preview state.
- Phase 1: pending
  - Add cached image-preview state to `ActiveOutputBundle` and remove old-image rereads from later reply rendering.
- Phase 2: pending
  - Add cached text head/tail preview state and stop reconstructing later previews from spilled file state.
- Phase 3: pending
  - Replace tests that depend on disk rereads with tests for the one-way contract.
- Phase 4: pending
  - Run full validation and update plan status for any remaining follow-on work.

## Locked Decisions

- Do not change the public files-mode bundle layout or path-disclosure contract as part of this refactor.
- Do not add tamper-detection logic for previously written bundle files.
- Do not rely on rereading previously written image files to build later visible replies.
- Keep preview state bounded in memory; do not retain the full spilled history in RAM.
- Preserve the current polling model and the existing distinction between inline preview content and the disclosed bundle path.

## Open Questions

- What exact text preview state should be cached in memory?
  - Likely one bounded head window plus one bounded tail window, but the final shape should preserve current preview wording and stream ordering.
- Should the preview cache live only on `ActiveOutputBundle`, or should `StagedTimeoutOutput` also keep a lightweight preview summary before a bundle is materialized?
- When an output-bundle directory or subdirectory disappears mid-session, what minimal recreation behavior is worth supporting for continued appends without expanding this slice into generic filesystem recovery logic?
- Can image preview caching reuse the existing `ReplyImage` payloads directly, or should it store a more compact internal representation?

## Next Safe Slice

- Add explicit preview-cache fields to `ActiveOutputBundle`.
- Update `append_image()` to maintain first/last preview images in memory when writing new image files.
- Change `compact_output_bundle_items()` so it renders later image previews from the in-memory cache instead of `load_output_bundle_*` disk reads.
- Add focused unit coverage in `src/server/response.rs` that proves later reply rendering does not depend on old bundle image files remaining on disk.
- After the image path is one-way, plan the text-preview cache slice separately inside this same document before changing text compaction behavior.

## Stop Conditions

- Stop if the slice requires retaining unbounded image or text history in memory.
- Stop if preserving the current visible preview contract requires a broader redesign of reply materialization than this plan assumes.
- Stop if the work starts changing the public bundle file format, tool descriptions, or polling contract.
- Stop if directory-recreation behavior grows into speculative recovery logic for states the public API does not need to support.

## Decision Log

- 2026-04-17: Decided that files-mode output bundles should be one-way from memory to disk. Disk is the server-owned history surface; memory owns the bounded preview needed for later replies.
- 2026-04-17: Scoped the first implementation slice to image preview caching, because the current image bundle reply path still rereads previously written files from disk.
- 2026-04-17: Kept the public bundle layout unchanged for this initiative so the refactor can land without redefining the client-facing files-mode contract.
