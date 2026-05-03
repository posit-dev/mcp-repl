# Claude Session Lifecycle And Integration

## Motivation

`mcp-repl` should behave predictably in Claude Code even though Claude does not
currently expose all client lifecycle events or agent identities through MCP.
The important workflows are session reset on `/clear`, reliable install/tool
visibility, and clear documentation of shared-session behavior for subagents.

## Target Scenarios

### `/clear` Resets The Runtime

Minimal task:

1. A user starts Claude Code with `mcp-repl` installed.
2. The agent creates runtime state through the `repl` tool.
3. The user runs `/clear` in Claude Code.
4. The next `repl` call uses a fresh worker session.

MCP does not define a `/clear` notification, and Claude does not currently send
one to MCP servers. A Claude-specific implementation would need to use Claude
hooks:

- a `SessionStart` hook injects the Claude session ID into the environment,
- `mcp-repl` records the active REPL control endpoint for that session ID,
- a `SessionEnd` hook looks up that endpoint and asks `mcp-repl` to restart the
  worker externally.

Codex already closes and restarts MCP connections on `/clear`, so this is a
Claude-specific lifecycle bridge rather than a general MCP requirement.

### Claude Subagents Share A REPL Session

Current Claude subagents share the same MCP connection as the main agent. Since
`mcp-repl` owns one long-lived runtime per MCP server connection, those subagents
also share the same REPL session.

There is no clean server-side fix under the current Claude MCP shape. Tool calls
do not include a stable agent or subagent ID. `toolUseId` might be correlated by
polling Claude transcript files, but that would be brittle and should not be
implemented as the happy path.

If Claude later sends a stable agent identity on MCP tool calls, revisit
per-agent worker routing. Until then, document the shared session in installer
output, the plugin skill, or Claude-specific guidance.

### Install And Protocol Drift Stay Covered

Minimal task:

1. A user runs `mcp-repl install --client claude`.
2. A fresh Claude Code session shows the installed R and Python tools.
3. A one-call smoke test succeeds for each installed interpreter.
4. A raw MCP `initialize` request with normal JSON-RPC shape succeeds.

The March 2026 install regression and initialize-handshake bug were both caused
by client/protocol drift not being covered by the same integration surface as
Codex. Future changes to install code, server initialization, and tool
description registration should include Claude coverage when practical.

### Claude Permission Snippets Stay Current

Claude Code permission syntax can change independently of `mcp-repl`. Generated
or documented Claude permission snippets should avoid known-deprecated patterns
such as the old `:*` suffix and should be checked against the current Claude
syntax when touched.

## Current Public Reset Surface

- `repl_reset` explicitly restarts the runtime.
- `\u0003` interrupts the current runtime request.
- `\u0004` resets the runtime and runs any remaining input in the fresh
  session.
- `q()` or EOF exits the runtime; the next request starts a fresh worker.
- Claude's `/mcp reconnect` exists as a user command, but there is no known
  programmatic hook for an MCP server to trigger it.

## Constraints

- Do not depend on MCP behavior that is not in the spec unless the feature is
  clearly Claude-specific and tested as such.
- Do not implement transcript polling to infer subagent identity.
- Do not broaden sandbox or network policy as part of lifecycle handling.
- Keep the runtime reset action server-owned. Hook scripts should only signal
  the already-running `mcp-repl` instance.

## Acceptance Shape

- Add install tests or smoke coverage showing Claude config generation still
  exposes the expected tools.
- Add a protocol test for the raw `initialize` request shape that previously
  failed.
- If `/clear` support is implemented, add hook fixture tests and a manual smoke
  scenario for Claude Code.
- Add skill or installer text that states Claude subagents share one REPL
  session under the current client behavior.

## Non-Goals

- Per-subagent REPL sessions for Claude before Claude exposes stable agent IDs.
- A generic MCP `/clear` protocol extension.
- Programmatically driving Claude's `/mcp reconnect` command from `mcp-repl`.
