# Future Work: Composable Tool Description Templates

## Summary

Potential follow-on: replace the current multi-file `repl` tool-description
matrix with one composable source document plus structured interpolation.

The intended direction is:

- keep one canonical core description for `repl`,
- render it at startup using Rust-native template/interpolation facilities,
- inject only the runtime-specific fields that actually vary.

The main dynamic axes we care about are:

- sandbox state and sandbox details,
- oversized-output / overflow behavior.

## Why This Matters

Today the server selects among multiple checked-in Markdown files for `repl`
tool descriptions:

- backend: R vs Python
- oversized-output mode: files vs pager

That keeps implementation simple, but it creates duplication and doc drift
pressure. Shared wording has to be copied across multiple files, and small
contract changes can require coordinated edits in every variant.

The current shape also makes it harder to expose runtime-specific information
such as sandbox state details without duplicating the rest of the document.

## Current Behavior

- `src/server.rs` selects one of four `include_str!` descriptions for `repl`.
- The descriptions live under `docs/tool-descriptions/`.
- `repl_reset` already has its own separate fixed document.

The current approach is explicit and compile-time friendly, but it is not the
best long-term authoring model.

## Intended Direction

- Keep one checked-in core `repl` description document.
- Represent variable sections with a small template mechanism instead of full
  duplicated files.
- Render the final description from:
  - the core document,
  - backend-specific fragments only where behavior genuinely differs,
  - runtime sandbox details,
  - runtime oversized-output behavior details.

This does not require a heavy external templating system. A small Rust-native
approach using format-time interpolation or a lightweight template crate is
enough as long as:

- the template syntax stays readable in the repo,
- rendered output remains deterministic,
- missing fields fail fast.

## Desired Outcomes

- One source of truth for shared `repl` wording.
- Runtime-specific sandbox details can be injected without forking the whole
  document.
- Overflow behavior can be described from one core template instead of keeping
  separate files-mode and pager-mode documents mostly in sync.
- Backend-specific differences stay narrow and obvious instead of being spread
  across four mostly duplicated files.

## Design Constraints

- Keep the checked-in source docs readable as docs, not as code soup.
- Prefer fail-fast rendering over fallback chains or silent omissions.
- Do not make the rendered description depend on unstable process state beyond
  the explicit runtime fields we choose to expose.
- Keep `repl_reset` separate unless the same composition mechanism clearly helps
  there too.

## Possible Follow-On Slice

- Define the minimum variable set needed by the current `repl` description:
  - backend-specific help/debugging lines,
  - sandbox summary/details,
  - oversized-output behavior text.
- Add one canonical template file under `docs/tool-descriptions/`.
- Replace the current `include_str!` matrix in `src/server.rs` with a render
  step that produces the final description string at startup.
- Add a docs contract test that renders every supported combination and checks
  for key expected sections.

## Non-Goals

- Redesigning the `repl` or `repl_reset` tool schema.
- Turning tool descriptions into a generic dynamic-content subsystem.
- Adding user-configurable arbitrary template logic.
