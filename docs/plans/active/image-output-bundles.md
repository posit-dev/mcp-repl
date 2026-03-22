# Image Output Bundles

## Summary

- Add a server-owned output bundle for oversized mixed text/image replies.
- Keep `transcript.txt` as worker-originated REPL text only.
- Add `events.log` as the ordered index over the full normalized reply stream.
- Keep the visible reply bounded with one truncation notice and the first/last image inline.

## Current Decisions

- Output bundle layout:
  - `transcript.txt`
  - `events.log`
  - `images/001.png`, `002.png`, ...
- `events.log` covers the full normalized stream, not just the omitted middle.
- `T` rows include both line and byte ranges into `transcript.txt`.
- `I` rows include only the relative image path.
- Output bundle compaction uses one merged pass over normalized reply items.
- The visible output-bundle reply keeps the first and last image inline.
- The truncation notice points to `events.log`.

## Guardrails

- Do not write image paths into `transcript.txt`.
- Do not add timestamps.
- Do not emit multiple truncation notices for one reply.
- Server meta text is still bounded by the global response budget.

## Next Slice

- Tighten merged compaction behavior and timeout coverage as follow-up slices if verification exposes gaps.
- Keep the docs and tests aligned with the output-bundle contract.
