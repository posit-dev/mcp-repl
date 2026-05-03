# Project-Local `mcp-repl` Config

## Summary

Potential future feature: support a project-local `mcp-repl-config.toml` file
so repository-specific `mcp-repl` options do not have to live in long MCP client
argument arrays.

## Motivation

- Better readability than long argument arrays in Codex or Claude MCP config.
- Easier project-specific configuration for package hosts, writable roots,
  local service access, and oversized-output behavior.
- A user-approval path for allowlist changes. MCP servers cannot prompt the
  client UI for permission, so allowlist edits need to happen through a surface
  the client already mediates, such as editing a project config file.
- Cleaner extension path for additional public options.

## Possible Shape

Supported project-local paths could include:

```text
.agents/mcp-repl-config.toml
.codex/mcp-repl-config.toml
.claude/mcp-repl-config.toml
```

Those directories are intentionally useful permission boundaries. In the
`workspace-write` sandbox, `.git`, `.codex`, and `.agents` are forced read-only
when present, and Claude project settings are likewise expected to sit behind
Claude's own file-edit permissions. Keeping allowlists there lets an agent ask
the user to approve the edit through the normal shell/edit permission flow
instead of giving `mcp-repl` a self-modifying allowlist tool.

Generated MCP client config could point at the file explicitly:

```toml
[mcp_servers.r]
command = "/Users/alice/.cargo/bin/mcp-repl"
args = [
  "--config-file", ".agents/mcp-repl-config.toml",
  "--interpreter", "r",
]
```

Example project config:

```toml
[server]
oversized_output = "files"

[sandbox]
mode = "inherit"

[sandbox.workspace_write]
network_access = true
writable_roots = ["./data", "./_cache"]

[permissions.network]
allowed_domains = ["cloud.r-project.org", "pypi.org", "files.pythonhosted.org"]
allow_local_binding = false
```

## Notes

- This is intentionally deferred.
- This should be opt-in. A project-local file should not be silently loaded from
  every repository unless the discovery and permission model are explicit.
- Do not add a model-facing MCP tool whose job is to rewrite the allowlist.
  Under current MCP clients, that tool cannot force a user approval prompt.
- The current `--config` CLI flag accepts ordered `key=value` overrides. Avoid
  ambiguous overloading; prefer a separate `--config-file <path>` unless a path
  form is made syntactically clear.
- Define project-root discovery before implementing automatic lookup. A global
  MCP client config should not accidentally pin every project to the config file
  from the repository where install happened.
- Project config must not silently broaden a sandbox policy supplied by the MCP
  client. It may set `mcp-repl` defaults or narrow behavior, but read-only and
  client-denied states must remain fail-closed.
- Network entries should use the same host-pattern contract as
  `--add-allowed-domain`: exact hosts, `*.example.com`, or `**.example.com`.
  URL/path entries such as `https://pypi.org/simple/` should stay invalid unless
  a later URL-policy design can enforce them.
- Choose one rule for resolving relative paths, such as relative to the config
  file directory.
- First slice: parser coverage, install coverage for one client, and a sandbox
  regression proving project config cannot escalate a client-provided read-only
  policy.
