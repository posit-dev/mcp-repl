# Future Work: REPL Description Extras (Recovered High-Signal Draft)

## Intent

Keep the default `repl` and `repl_reset` tool descriptions short and schema-focused.

Preserve the richer high-signal operational guidance from the old long-form tool description in an optional layer (skill or backend-specific extras) so we can bring it back without bloating default tool metadata.

## Scope of this draft

This draft is the recovery set for high-signal content removed during the `write_stdin` -> `repl` surface cleanup.

It should be treated as source material for:
- backend-specific optional description extras
- a dedicated REPL skill
- dynamic runtime selection of extras by active backend

## Recovered backend-selection guidance

- Default backend: `r`.
- CLI backend selection: `mcp-repl --interpreter r|python`.
- Environment backend selection: `MCP_REPL_INTERPRETER=r|python`.

## Recovered shared guidance

### Operating model

- Use the REPL for short inspect-run loops when direct execution is faster than reasoning.
- Reuse session state when it helps iteration speed, but do not rely on persistence for correctness.
- Prefer explicit reset when state is stale, inconsistent, or too large.
- Avoid speculative fallback chains; fail fast and inspect actual runtime state.

### Memory hygiene

- Long-lived sessions can retain large objects and hold significant memory while idle.
- Reset aggressively after large one-off explorations.
- Treat idle memory hoarding as a bug in workflow, not a normal steady state.

### Polling and long-running execution

- Use short non-blocking/near-non-blocking calls to launch long work, then poll.
- Treat timeout return as partial progress, not cancellation.
- Timeout replies surface explicit busy status markers (`<<console status: busy, write_stdin timeout reached; elapsed_ms=...>>`).
- While work is active, reject/discard concurrent non-empty input and poll until completion.
- Empty-input poll while idle can return `<<console status: idle>>`.
- After completion, resume normal interactive flow.

## Recovered R-specific guidance

### Primary use cases

- Quick computation and grounded inspection.
- Package behavior verification.
- Small code-path validation.
- Intermediate value checks before broader refactors.

### In-band docs/manual discovery

- `?topic`, `help(...)`
- `help(package = "pkg")`, `help.search(...)`, `library(help = "pkg")`, `package?pkg`
- `vignette(package = "pkg")`, `vignette("topic", package = "pkg")`
- `RShowDoc("R-exts")`
- `getAnywhere(name)`

### Development loop

- After package edits, run `devtools::load_all()` before interactive verification.
- Keep repeated setup in scripts and `source("file.R")` for reproducible loops.

### Debugging playbook

- Use `browser()`, `debug()`, `debugonce()`, `trace()` to inspect execution points.
- In debugger frames, inspect with `ls.str()` and `sys.calls()`.
- In browser mode, `?` prints debugger commands.
- Use browser-driven development:
  - start with a minimal stub and `browser()`
  - trigger real call sites
  - inspect inputs (`str(...)`)
  - implement one expression at a time while observing intermediate state

### Complex transformation workflow

- For data wrangling and string-heavy logic (`grep`, `sub`, `gsub`, regex unpacking), avoid guessing.
- Step through concrete examples interactively.
- Validate assumptions before generalizing transformations.

### Contracts and preconditions

- State assumptions at function entry.
- Prefer small `stopifnot()` contracts in public APIs.

## Recovered Python-specific guidance

### In-band docs and inspection

- `help()`, `dir()`, `pydoc.help`
- Inspect concrete object state/types before structural changes.

### Debugging playbook

- Use `breakpoint()` and `pdb.set_trace()`.
- Run step/inspect/continue loops on real failing paths.

### Contracts and preconditions

- Prefer typed interfaces plus small explicit assertions.
- Avoid broad defensive fallback trees when a concrete error can clarify intent.

## Recovered pager guidance

### Pager semantics

- Pager activates when output exceeds page budget.
- While pager is active, backend input is blocked.
- Empty input advances one page.
- Non-empty pager commands must be `:`-prefixed.
- Non-empty non-command input is rejected while pager is active.
- Backend prompt is suppressed during pager mode and restored after exit.
- Pager output de-duplicates already shown content within a pager session.
- Pager mode can emit compact input summaries such as `[mcp-repl] input: ... [TRUNCATED]`; this is not a backend prompt.

### Pager commands

- next page: empty input
- quit: `:q`
- search: `:/pattern`
- next match: `:n`
- emit all remaining: `:a`
- help: `:help`

## Recovered image guidance

- Plot/image output is first-class content in tool responses.
- Re-run plot commands after state edits to verify deltas.
- Prefer deterministic plotting where possible (fixed seeds, explicit dimensions).

### R plot controls

- `options(console.plot.width = ..., console.plot.height = ...)`
- `options(console.plot.units = "in" | "cm" | "mm" | "px")`
- `options(console.plot.dpi = ...)` (alias: `console.plot.res`)

## Recovered sandbox guidance

- If shell execution is policy-blocked, try equivalent commands through the REPL backend when supported.
- Common workflows where this matters include test runs, package build/check loops, and rendering workflows.

## Recovered reset/interrupt/session guidance

### Control prefixes

- `\u0003` (Ctrl-C): best-effort interrupt, then run remaining input in current session.
- `\u0004` (Ctrl-D): reset session, then run remaining input in fresh session.
- Optional separator newline after control prefix is accepted.

### Reset tool semantics

- `repl_reset` is explicit state reset.
- Prefer `repl_reset` when the intent is session lifecycle control rather than payload execution.

### Session exit and crashes

- EOF or runtime exit leads to worker respawn for the next request.
- `quit("no")`, `quit()`, and `quit("yes")` end the current session; the next request runs in a fresh worker.
- Save-workspace prompts in R are auto-answered `no` to avoid hangs.
- Mid-request worker exit/crash should return captured output and a terminal error line.

## Recovered feedback loop

- If tool behavior is confusing, surprising, or error-prone, report it and capture concrete wording improvements for the extras draft.

## Delivery plan

1. Keep default backend tool descriptions short.
2. Add optional extras selected by backend at runtime.
3. Add a dedicated skill that includes these full recipes and debugging patterns.
4. Add a small mechanism to attach extras only when requested by the client/profile.

## Open questions

- Config path: CLI flag vs env var vs config file vs client profile.
- Attachment model: append to tool doc vs separate instruction channel.
- Backend growth: how to add parallel extras for future backends (for example Julia) without duplicating shared guidance.
