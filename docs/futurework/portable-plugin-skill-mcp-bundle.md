# Portable Plugin Skill MCP Bundle

## Summary

Package `mcp-repl` as a client plugin that installs both:

- an MCP server configuration for the `mcp-repl` tools,
- a skill that teaches agents when and how to use those tools.

The preferred direction is a single portable plugin directory with shared
root-level `skills/` and `.mcp.json`, plus both Codex and Claude manifest
flavors pointing at the same files.

This is a future distribution surface, not a replacement for
`mcp-repl install`. The current installer owns the known-good direct MCP
configuration path. A plugin should start as a small, testable wrapper around
the same executable and the same tool behavior.

## Research Findings

This note comes from a local review of Codex plugin/skill support and current
Claude plugin documentation on 2026-04-25.

### `.agents` vs `.codex`

- Codex configuration and plugin cache state are still Codex-owned under
  `~/.codex` or `$CODEX_HOME`.
- Codex reads user-installed skills from `~/.agents/skills` in addition to the
  older `$CODEX_HOME/skills` location.
- Codex discovers repo skills from `.agents/skills`.
- `~/.agents/.skill-lock.json` is not a Codex plugin manifest. It is Skills CLI
  state from the wider Agent Skills ecosystem. The observed upstream schema is
  `vercel-labs/skills` `src/skill-lock.ts`, version 3, with entries keyed by
  skill name and fields such as `source`, `sourceType`, `sourceUrl`,
  `skillPath`, `skillFolderHash`, `installedAt`, `updatedAt`, and optional
  `pluginName`. Treat it as installer/update metadata, not as a package
  declaration for `mcp-repl`.

### Plugin Format Support

Plugins are not OpenAI-only. Claude Code has a plugin system too, but the
formats are not one universal spec.

Codex-native layout:

- plugin manifest: `.codex-plugin/plugin.json`
- marketplace manifest: `.agents/plugins/marketplace.json`

Claude-native layout:

- plugin manifest: `.claude-plugin/plugin.json`
- marketplace manifest: `.claude-plugin/marketplace.json`

Current Codex compatibility is useful for a shared prototype:

- Codex discovers both `.codex-plugin/plugin.json` and
  `.claude-plugin/plugin.json`.
- Codex discovers both `.agents/plugins/marketplace.json` and
  `.claude-plugin/marketplace.json`.
- Codex plugin manifests currently model `skills`, `mcpServers`, and `apps` as
  path fields. For `mcpServers`, use a path to a JSON file rather than relying
  on inline MCP config.
- Codex loads the default `skills/` directory and also adds the manifest
  `skills` path when present.
- Codex loads `.mcp.json` by default, or the file referenced by `mcpServers`.
  The file may use a wrapped `{"mcpServers": {...}}` shape or a flat server map.
- Codex normalizes relative `cwd` values in plugin MCP server config against the
  plugin root.

Claude compatibility constraints:

- Claude plugins use the same root-level `skills/` and `.mcp.json` concepts.
- Claude supports `.mcp.json` at the plugin root or inline `mcpServers` in
  `plugin.json`.
- Claude documents `${CLAUDE_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_DATA}` for
  plugin-relative paths and persisted plugin data.
- Claude plugin components must live at the plugin root, not inside
  `.claude-plugin/`.

The common subset is therefore:

- root-level `skills/<skill>/SKILL.md`,
- root-level `.mcp.json`,
- a manifest that points to `./skills/` and `./.mcp.json`,
- no reliance on inline `mcpServers`,
- no reliance on `${CLAUDE_PLUGIN_ROOT}` in the shared `.mcp.json`,
- no reliance on Codex-only sandbox metadata in the shared `.mcp.json`.

Marketplace files are not the common subset. Keep the plugin directory shared,
but expect separate marketplace declarations for Codex and Claude if the plugin
needs to be distributed through both plugin managers.

## Intended Shape

Use one plugin root:

```text
mcp-repl-plugin/
  .codex-plugin/plugin.json
  .claude-plugin/plugin.json
  skills/
    mcp-repl/
      SKILL.md
  .mcp.json
```

Use the same minimal manifest shape for both clients when possible:

```json
{
  "name": "mcp-repl",
  "version": "0.1.0",
  "description": "REPL skills plus MCP tools.",
  "skills": "./skills/",
  "mcpServers": "./.mcp.json"
}
```

Use wrapped MCP config so the same `.mcp.json` can be consumed by both clients:

```json
{
  "mcpServers": {
    "r": {
      "type": "stdio",
      "command": "mcp-repl",
      "args": ["--interpreter", "r"]
    },
    "python": {
      "type": "stdio",
      "command": "mcp-repl",
      "args": ["--interpreter", "python"]
    }
  }
}
```

This shared config assumes `mcp-repl` is already invokable by the client
process. That can be through `PATH`, a package runner, or a wrapper command, but
the first prototype should not depend on plugin-relative executable paths.

The skill should stay declarative and client-neutral:

```markdown
---
name: mcp-repl
description: Use when iterating in persistent R or Python REPL sessions through mcp-repl.
---

Use the bundled `mcp-repl` MCP tools for interactive R and Python runtime
inspection, plotting, help, debugging, and short verification loops.
Prefer these tools over ad hoc shell execution when session state, plots, or
runtime-specific help are useful.
```

## Portability Rule

The most portable plugin uses an MCP server executable that is already
invokable through one of these routes:

- `PATH`,
- `npx`,
- `uvx`,
- `docker`.

For `mcp-repl`, the first slice should assume the binary is available on `PATH`
or installed separately by the existing installer. A plugin should not silently
replace the current install path until the cross-client packaging behavior is
verified.

Avoid putting client-specific tool names into the shared skill. Codex and
Claude often expose MCP tools through similar names, but the skill should say
"use the bundled `mcp-repl` MCP tools" rather than hard-code a particular
`mcp__server__tool` spelling.

## Plugin-Relative Server Code

Bundling the MCP server implementation inside the plugin is weaker
cross-client ground.

Claude documents plugin-root substitution through `${CLAUDE_PLUGIN_ROOT}`.
Codex currently handles plugin-relative MCP config mainly by resolving a
relative `cwd` against the plugin root.

If `mcp-repl` needs plugin-root-relative server paths, keep the skill shared but
split the MCP config files:

```text
mcp-repl-plugin/
  .codex-plugin/plugin.json
  .claude-plugin/plugin.json
  skills/
    mcp-repl/
      SKILL.md
  .mcp.codex.json
  .mcp.claude.json
```

```json
{
  "name": "mcp-repl",
  "skills": "./skills/",
  "mcpServers": "./.mcp.codex.json"
}
```

```json
{
  "name": "mcp-repl",
  "skills": "./skills/",
  "mcpServers": "./.mcp.claude.json"
}
```

Split only for concrete path-resolution differences. Keep one shared `.mcp.json`
when the server is externally invokable.

Likely reasons to split:

- The server command must reference a binary or script inside the plugin
  directory.
- The server needs a plugin-persistent data directory.
- The Codex config needs `--sandbox inherit` while Claude needs explicit default
  sandbox behavior.
- The client requires different environment variables or approval metadata.

For plugin-relative code, a Codex-specific config can use a relative `cwd` that
Codex resolves against the plugin root. A Claude-specific config can use
`${CLAUDE_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_DATA}`. Do not assume either
client expands the other client's placeholders.

## Relationship To Current Install

The current `mcp-repl install` command writes client-specific MCP config for
Codex and Claude. A plugin would be a higher-level distribution surface:

- the MCP config starts the same tools,
- the bundled skill carries richer operational guidance,
- users can install one artifact instead of separately configuring tools and
  copying instructions.

This should not remove the existing installer. The plugin path is additive
until it is proven to cover the same client-specific sandbox and oversized-output
defaults.

## Prototype Plan

1. Create a checked-in `plugins/mcp-repl/` prototype with shared `skills/` and
   shared `.mcp.json`.
2. Start with `command = "mcp-repl"` and document that the binary must already
   be installed.
3. Move the high-signal guidance from
   `docs/futurework/repl-tool-description-extras.md` into the first skill draft.
4. Add `.codex-plugin/plugin.json` and `.claude-plugin/plugin.json` with the
   same `name`, `description`, `skills`, and `mcpServers` fields.
5. Add a Codex local marketplace entry at
   `.agents/plugins/marketplace.json` in the test repository. Point
   `source.path` at `./plugins/mcp-repl`.
6. Add a Claude local marketplace entry at
   `.claude-plugin/marketplace.json` in the same test repository, or use the
   Claude CLI's local marketplace add flow.
7. Install from both plugin managers and verify:
   - the skill appears,
   - the MCP servers start,
   - the R and Python tools are visible,
   - a minimal one-call REPL smoke test works for each interpreter,
   - uninstall/disable leaves the existing `mcp-repl install` path unaffected.
8. Only after that, evaluate whether a packaged executable route such as `npx`,
   `uvx`, or a release-binary wrapper is worth supporting.

## Example Marketplace Stubs

Codex repo-local marketplace:

```json
{
  "name": "local-mcp-repl",
  "interface": {
    "displayName": "Local mcp-repl"
  },
  "plugins": [
    {
      "name": "mcp-repl",
      "source": {
        "source": "local",
        "path": "./plugins/mcp-repl"
      },
      "policy": {
        "installation": "AVAILABLE",
        "authentication": "ON_INSTALL"
      },
      "category": "Developer Tools"
    }
  ]
}
```

Claude repo-local marketplace:

```json
{
  "name": "local-mcp-repl",
  "owner": {
    "name": "mcp-repl maintainers"
  },
  "plugins": [
    {
      "name": "mcp-repl",
      "source": "./plugins/mcp-repl",
      "description": "REPL skills plus MCP tools."
    }
  ]
}
```

These marketplace files are distribution adapters. The shared artifact is still
the plugin root under `plugins/mcp-repl/`.

## Source References

- Codex plugin docs:
  `https://developers.openai.com/codex/plugins`
- Codex plugin build docs:
  `https://developers.openai.com/codex/plugins/build`
- Claude plugin docs:
  `https://docs.claude.com/en/docs/claude-code/plugins`
- Claude plugin marketplace docs:
  `https://docs.claude.com/en/docs/claude-code/plugin-marketplaces`
- Claude plugin reference:
  `https://docs.claude.com/en/docs/claude-code/plugins-reference`
- Codex source areas reviewed:
  - `codex-rs/core-skills/src/loader.rs`
  - `codex-rs/utils/plugins/src/plugin_namespace.rs`
  - `codex-rs/core-plugins/src/marketplace.rs`
  - `codex-rs/core-plugins/src/manifest.rs`
  - `codex-rs/core-plugins/src/loader.rs`

## Open Questions

- Should the plugin expose one server per interpreter, or one server with a
  selected default interpreter?
- How should plugin install preserve Codex `--sandbox inherit` while using
  explicit sandbox defaults for Claude?
- Should the plugin be generated from the existing install code, or checked in
  as static assets?
- What should be the release vehicle: repo artifact, package registry, or
  generated files from `mcp-repl install`?
- Should the shared skill be terse and client-neutral, with richer client-
  specific guidance in separate optional skills, or should one skill carry all
  operational details?
- Should plugin packaging be tested in CI with fake Codex/Claude plugin roots,
  or only by live manual smoke tests until plugin behavior stabilizes?

## Non-Goals

- Replacing the existing `mcp-repl install` command in the first slice.
- Bundling R or Python themselves.
- Encoding client-specific path hacks in the shared skill.
- Assuming plugin-relative executable paths are portable without testing both
  clients.
