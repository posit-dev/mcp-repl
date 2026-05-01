# Project-Local `mcp-repl` Config

## Motivation

Projects should be able to declare their expected `mcp-repl` behavior without
requiring every MCP client config to carry a long, hand-maintained argument
array. A checked-in project config would let a repository say which package
hosts, local services, writable roots, output mode, and other supported options
make sense for that project.

Minimal task:

1. A repository contains `.agents/mcp-repl-config.toml`,
   `.codex/mcp-repl-config.toml`, or `.claude/mcp-repl-config.toml`.
2. A user runs `mcp-repl install --client codex` or `mcp-repl install --client
   claude` from that repository.
3. The installer writes the MCP client entry so the server loads the
   project-local config for that repository at startup.
4. The agent calls `repl` through the MCP client.
5. The server applies the project-local settings consistently for that
   project's worker, while unrelated projects keep their own settings.

This is a usability feature and a configuration-sharing feature. It should not
weaken the sandbox policy supplied by the MCP client or make project files
implicitly trusted to grant broader permissions than the client allowed.

## Candidate Shape

Prefer one canonical file name:

```text
.agents/mcp-repl-config.toml
.codex/mcp-repl-config.toml
.claude/mcp-repl-config.toml
```

The implementation should choose an explicit discovery contract. A reasonable
starting point is:

- prefer a path passed by the MCP client or install-generated args,
- otherwise look for `.agents/mcp-repl-config.toml` in the server working
  directory,
- allow `.codex/mcp-repl-config.toml` and `.claude/mcp-repl-config.toml` as
  client-specific paths only if the discovery order is documented and tested.

Do not let a global MCP client config accidentally pin every project to the
config file from the repository where install happened. If install writes an
absolute config path, that behavior should be explicit and probably opt-in.
For default project-local behavior, the server needs a reliable active project
root from the MCP client, its working directory, or another documented source.

Example config:

```toml
[server]
oversized_output = "files"

[sandbox.workspace_write]
network_access = true
writable_roots = ["./data", "./_cache"]

[permissions.network]
allowed_domains = [
  "cloud.r-project.org",
  "pypi.org",
  "files.pythonhosted.org",
]
denied_domains = []
allow_local_binding = false
```

The schema should use the same terms as the existing CLI/config overrides:
`sandbox_mode`, workspace-write network access, writable roots, managed network
allowed/denied domains, local binding, oversized-output mode, interpreter
selection, and any future public options that would otherwise require long MCP
client argument arrays.

Interpreter selection needs special care because install creates separate R and
Python MCP server entries. Either keep interpreter selection in the MCP client
entry, or make the project config schema explicitly server-specific.

## Current Shape

- `src/install.rs` writes Codex `config.toml` and Claude `.claude.json` MCP
  entries with explicit argument arrays.
- Codex install currently defaults to `--sandbox inherit --oversized-output
  files`.
- Claude install currently writes an explicit sandbox mode because Claude does
  not provide Codex sandbox metadata.
- The current `--config` CLI flag accepts ordered `key=value` overrides such as
  `permissions.network.allowed_domains=[...]`. It does not accept a file path.
- Existing sandbox parsing logic is in `src/sandbox_cli.rs`. It validates
  supported keys and applies operations in argument order.
- `.codex` and `.agents` are already treated as protected project metadata in
  sandbox write policy. A project-local config in those directories should be
  read by the server before worker sandboxing, not written by the worker.

## Design Constraints

- Do not overload the existing `--config key=value` form ambiguously. Use a
  separate flag such as `--config-file <path>` or make `--config` path support
  syntactically unambiguous and well tested.
- Preserve MCP-client sandbox authority. A project config may narrow behavior or
  add mcp-repl defaults, but it should not silently escalate a read-only or
  client-denied sandbox into broader access.
- Keep layering deterministic. Document whether CLI args override project
  config, project config overrides install defaults, and how repeated network
  allowlist entries merge or replace.
- Define the active project root explicitly. Do not infer it from a user home
  config path when the agent is operating in a different repository.
- Normalize relative paths against the config file directory or the server
  working directory, and choose exactly one rule.
- Treat `.agents`, `.codex`, and `.claude` as project metadata. Do not require
  the worker to write or mutate these files.
- Keep the first schema small. Avoid a general-purpose config language; expose
  only public options that already have a stable CLI or documented behavior.

## Acceptance Shape

- Add a parser test for a minimal `mcp-repl-config.toml`.
- Add a layering test that proves CLI args and project config combine in the
  documented order.
- Add install tests for Codex and Claude that verify generated MCP client
  configs point at the project-local config when one is present.
- Add a sandbox test proving project config cannot broaden a client-provided
  read-only policy into workspace-write or full access.
- Add a docs example that shows the config file alongside the generated MCP
  client entry.
