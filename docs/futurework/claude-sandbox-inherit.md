# Claude Sandbox Inherit

## Motivation

Claude users should not have to describe the same sandbox policy twice. If a
project already has Claude sandbox settings, `mcp-repl` should be able to use
the equivalent policy for its worker when Claude starts the MCP server.

Minimal task:

1. A project has Claude settings in `.claude/settings.json` or
   `.claude/settings.local.json`.
2. The user runs `mcp-repl install --client claude`.
3. The generated MCP config starts `mcp-repl` in a Claude-inherit mode.
4. The server reads the effective Claude sandbox shape for the active project.
5. The worker uses the corresponding `mcp-repl` sandbox and managed-network
   policy, or fails closed when the shape cannot be represented safely.

## Current Shape

- Claude install currently writes `.claude.json` MCP server entries and updates
  `.claude/settings.json` permissions so Claude can call the generated tools.
- Claude install uses an explicit `mcp-repl` sandbox mode because Claude does
  not send `_meta["codex/sandbox-state-meta"]` per-tool-call sandbox metadata to
  MCP servers.
- Claude's public settings shape is JSON, not TOML. Project settings live under
  `.claude/settings.json` and `.claude/settings.local.json`, and sandbox options
  are nested under `sandbox`.
- Claude sandbox settings include network-related fields such as local binding
  and proxy ports. Filesystem and network intent may also interact with Claude
  permission rules.

Reference: <https://docs.claude.com/en/docs/claude-code/settings>

## Notes

- This should be a separate feature from managed-network install defaults.
- Claude's sandbox implementation and documentation are useful prior art for
  this feature. Re-inspect the current Claude source or docs when implementing
  instead of preserving stale assumptions about settings shape or permission
  semantics.
- Decide whether to add a Claude-specific inherit mode, for example
  `--sandbox inherit-claude`, or to extend `--sandbox inherit` with a documented
  client source.
- Claude inheritance is likely startup/project scoped, not per-tool-call scoped,
  unless Claude later sends sandbox metadata with MCP tool calls.
- Do not silently broaden permissions. If the Claude settings shape cannot be
  mapped to `mcp-repl` sandbox state, fail closed or require explicit
  `mcp-repl` config.
- Preserve Claude settings precedence. If implementation reads settings files
  directly, it needs a tested merge order for user, project, local project, and
  managed settings, or it needs to consume an already-resolved Claude-provided
  shape.
- Keep the first slice small. A reasonable first pass could map sandbox enabled
  state, local binding, and managed proxy ports before attempting full
  permission-rule parity.

## Acceptance Shape

- Add fixture tests for representative Claude settings files.
- Add an install test showing `mcp-repl install --client claude` can write the
  selected Claude-inherit mode.
- Add a sandbox test proving unsupported or broader-than-representable Claude
  settings fail closed.
- Add a runtime smoke test showing a supported Claude sandbox setting affects
  the worker sandbox as expected.
