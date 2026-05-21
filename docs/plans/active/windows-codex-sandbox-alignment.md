# Windows Codex Sandbox Alignment

## Motivating Scenario

An MCP client starts `mcp-repl` on Windows with Codex sandbox metadata. A
default `workspace-write` REPL should run in a Codex-style offline sandbox
identity, with MCP REPL's session temp directory and runtime read/write
carve-outs handled internally. Later, the same machinery should power
`mcp-repl sandbox-exec <command>` without inventing a separate policy model.

## Current Slice

- Prefer Codex `permissionProfile` metadata over legacy `sandboxPolicy`.
- Bridge only the safe legacy-equivalent profiles for now:
  `read-only` with restricted network, `workspace-write` with restricted
  network, and explicit full access.
- Name MCP REPL's intended Windows sandbox identities separately from Codex:
  `McpReplSandboxOffline` and `McpReplSandboxOnline`.
- Record that the present worker launcher is still the legacy restricted-token
  backend, so future elevated setup and runner work has a clear replacement
  boundary.

## Constraints

- Do not silently broaden Codex split filesystem or network profiles. Reject
  deny-read, minimal-read, glob-scanned, external, and managed full-network
  profiles until MCP REPL enforces them directly.
- Preserve MCP REPL-specific internal writable/read roots: session temp,
  R/Python runtime paths, package/library reads, plot/image outputs, debug
  logs, and output bundles.
- Avoid collisions with Codex's Windows users, firewall rules, WFP filters, and
  setup markers if MCP REPL vendors or forks the implementation.

## Next Slice

- Vendor or wrap Codex's elevated setup and command-runner implementation with
  MCP REPL-specific user/rule names.
- Refresh setup before worker launch without surprise UAC prompts during a
  normal tool call.
- Route restricted-network launches through the offline identity and explicit
  full-network launches through the online identity.
- Expose `mcp-repl sandbox-exec` as a thin public command over the same launch
  plan.
