# Future Work: Offline Manual Surfaces For REPL Backends

## Summary

Potential follow-on: revisit how large local manuals are exposed to the model,
starting with R `RShowDoc()` and extending to Python manuals.

The current in-band flow works, but it is not the best model-facing surface for
large structured documentation.

This needs design work around:

- better bundle materialization for large manual output,
- whether manuals should be presented as one file, a richer output bundle, or a
  directory-shaped surface,
- how to make equivalent offline Python manuals available,
- and how to do all of that without assuming the model itself has network
  access, even when the MCP server does.

## Why This Matters

Today the R backend intercepts `RShowDoc()` / `browseURL()` flows and renders
local manual HTML directly into the REPL response.

That is enough to avoid opening a browser or relying on a help server, but it
has limitations:

- large manuals are still funneled through a text-first REPL response path,
- the current output shape is not obviously the best way for a model to inspect
  or navigate manual structure,
- there is no parallel "offline manual bundle" story for Python,
- and current behavior can implicitly assume that in-band rendering is the only
  useful offline access pattern.

For model-oriented use, local manuals are more like inspectable datasets than
ordinary console output.

## Current Behavior

- `RShowDoc()` currently resolves local manual files and routes them through the
  REPL/browseURL path instead of launching an external browser.
- Public tests cover that behavior and treat HTML rendering into the REPL as the
  current contract.
- The REPL already has output-bundle machinery, but manuals are not yet treated
  as a first-class documentation bundle surface.
- Python currently documents in-band help flows (`help()`, `pydoc.help`) but
  does not expose an equivalent offline-manual surface analogous to R manuals.

## Intended Direction

- Treat large local manuals as a separate kind of inspectable output, not only
  as a long console transcript.
- For R:
  - revisit `RShowDoc()` so the model sees a better offline surface than raw
    REPL-rendered HTML alone,
  - consider materializing a richer output bundle or directory-oriented manual
    view that preserves structure and anchors.
- For Python:
  - design an equivalent offline manual surface so the model can inspect Python
    documentation in a way that is parallel to R manuals,
  - prefer local/manual-backed access over network assumptions.

## Key Constraints

- Do not assume the model has network access, even if the MCP server does.
- Prefer local/manual-backed documentation over live internet fetches.
- Keep the public REPL interaction model understandable; do not turn ordinary
  help calls into an opaque side channel without a clear contract.
- Preserve a fail-fast story when the relevant local manuals are unavailable.

## Possible Directions

### 1. Better manual output bundle

- Materialize a dedicated bundle for large manuals.
- Preserve the source file path, section anchors, and a structured index.
- Let the inline REPL reply show only a compact preview plus the bundle path.

### 2. Directory-shaped manual surface

- Present manuals as a directory containing:
  - the rendered source HTML/text,
  - an index of anchors/sections,
  - optional extracted plain-text shards for search-friendly inspection.
- This may be easier for models to inspect than one monolithic HTML dump.

### 3. Backend-parallel offline docs

- Keep R and Python aligned at the product level:
  - both should have an offline documentation story,
  - both should avoid relying on a browser or internet access,
  - both should expose inspectable local artifacts when the content is too
    large or too structured for a normal inline reply.

## Relationship To Other Work

- This is separate from `docs/futurework/composable-tool-descriptions.md`.
  Tool descriptions may tell the model that manuals are available, but they do
  not define how manuals are surfaced.
- This is also separate from oversized-output bundle architecture in general.
  Manuals may reuse bundle machinery, but the product question here is the
  manual/documentation UX for the model.

## Possible Follow-On Slice

- Audit the current `RShowDoc()` / `browseURL()` rendering path and document the
  concrete failure modes or friction points.
- Prototype one offline manual bundle shape for R manuals.
- Decide whether the model should receive:
  - a single bundle path,
  - a directory path,
  - or a compact inline card plus inspectable artifacts.
- Identify one concrete source for local Python manuals and prototype the same
  inspection surface without relying on network access.

## Non-Goals

- Replacing ordinary small help output such as `?topic` or `help(len)` when the
  inline flow is already sufficient.
- Making live internet browsing the default answer for backend manuals.
- Redesigning the entire output-bundle subsystem in the same slice.
