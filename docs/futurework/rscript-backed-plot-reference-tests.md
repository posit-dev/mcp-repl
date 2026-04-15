# Future Work: `Rscript`-Backed Plot Reference Tests

## Summary

Potential follow-on cleanup: stop treating plot image content as snapshot material in
`tests/plot_images.rs`.

Instead, for R plot/image coverage:

- shell out to `Rscript` to render the expected image into a temp path
- capture the image emitted through `mcp-repl`
- compare the produced image from `mcp-repl` against the `Rscript` reference render
- keep snapshots focused on transcript structure, image count, MIME type, bundle layout,
  and other non-binary reply details

This is not the current repository contract. It is a future testing direction.

## Current State

`tests/plot_images.rs` already has part of this shape:

- `reference_image_script(name, path)` defines a few reference renders
- `assert_reference_image(name, bytes)` shells out to `Rscript` and compares bytes
- `response_snapshot()` still serializes image-bearing replies into `.snap` files, with
  image `data` replaced by a summary string such as `800x600`

That is better than snapshotting raw base64 payloads, but it still mixes two separate
concerns:

- binary image correctness
- transcript/reply structure

## Desired Direction

Make the reference render the primary correctness check for R plot tests.

Preferred shape:

- every R plot case that cares about image correctness defines a reference `Rscript` render
- the reference script writes to a temp PNG path
- the test decodes the `mcp-repl` image payload and compares it to the file rendered by
  `Rscript`
- `.snap` files do not store image bytes, hashes, or dimension summaries unless a specific
  non-binary image summary is still needed for a public reply contract

In other words, the snapshot should answer:

- how many images were emitted
- in what order
- alongside what text/bundle behavior

The reference render should answer:

- whether the actual image content matches what plain R would have produced

## Why

- Reduces snapshot churn from image-derived fields.
- Makes failures easier to read: the test failed because `mcp-repl` rendered a different
  image than `Rscript`, not because a snapshot blob changed.
- Keeps binary validation tied to the same local R installation and graphics device family
  that the test environment is already using.
- Separates transcript contracts from image-rendering contracts.

## Suggested Refactor Shape

Potential implementation direction in `tests/plot_images.rs`:

- expand `reference_image_script(name, path)` into the main fixture table for R plot cases
- have `step_snapshot()` / `response_snapshot()` redact image payloads completely after the
  explicit reference comparison has run
- keep snapshot assertions for visible text, image count, MIME type, and output-bundle
  disclosure behavior
- use the existing `assert_reference_image()` flow as the starting point rather than adding a
  second image-validation mechanism

## Important Details

- The reference render should use explicit device settings such as:
  - `grDevices::png(filename = ..., width = 800, height = 600, res = 96)`
- The `Rscript` helper should render into a temp path owned by the test, not into a checked-in
  fixture file.
- Multi-image or update-heavy replies may still need separate structural snapshot coverage even
  when the final rendered image is reference-checked.

## Open Questions

- Whether exact byte equality is the right long-term comparison, or whether decoded pixel
  equality would be more robust if PNG metadata starts to vary.
- Whether every plot case needs a reference render, or whether some cases should remain
  structure-only tests.
- Whether `tests/python_plot_images.rs` should eventually adopt an analogous reference-render
  pattern with a standalone Python renderer.
