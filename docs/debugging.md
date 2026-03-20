# Debugging MCP REPL

`mcp-repl` has built-in debugging at a few different layers:

- Internal event logs from inside `mcp-repl`
- Worker startup and sandbox diagnostics
- An external trace proxy that captures the raw stdio traffic between the client and `mcp-repl`

Use the built-in logs when you want to understand what `mcp-repl` thinks happened. Use the external proxy when you need to see the exact bytes that crossed the wire.

## Built-in event logs

Enable per-startup JSONL logs with either:

- `mcp-repl --debug-dir /path/to/debug-root`
- `MCP_REPL_DEBUG_DIR=/path/to/debug-root`

Each startup creates a fresh session directory under that root. `mcp-repl` writes:

- `events.jsonl` with startup metadata, tool calls, and sandbox custom request events
- `startup.log` for server-side startup trace lines
- `worker-startup.log` for worker-side startup trace lines
- `sandbox-state.jsonl` for sandbox policy and sandbox-state update payloads

Example:

```sh
mkdir -p /tmp/mcp-repl-debug
MCP_REPL_DEBUG_DIR=/tmp/mcp-repl-debug mcp-repl --interpreter r
```

## Startup diagnostics

Use `MCP_REPL_DEBUG_DIR` or `--debug-dir` when the worker fails early or startup feels slow. The startup trace lines go into `startup.log` and `worker-startup.log` inside the session directory.

Example:

```sh
MCP_REPL_DEBUG_DIR=/tmp/mcp-repl-debug mcp-repl --interpreter python
```

## MCP and sandbox tracing

These switches are useful when the client is sending custom sandbox updates or when the sandbox policy is the thing you are debugging.

- `MCP_REPL_DEBUG_DIR=/path/to/debug-root` writes `sandbox-state.jsonl` inside the session directory
- `MCP_REPL_KEEP_SESSION_TMPDIR=1` keeps the worker session temp directory after exit so you can inspect it
- macOS only: `MCP_REPL_SANDBOX_LOG_DENIALS=1` prints collected sandbox denials when the worker exits

Example:

```sh
MCP_REPL_DEBUG_DIR=/tmp/mcp-repl-debug mcp-repl --sandbox inherit
```

## Interactive debug REPL

`--debug-repl` runs `mcp-repl` as a local interactive driver for the worker instead of as an MCP server. This is the fastest way to reproduce REPL behavior without involving a client.

Start it with:

```sh
mcp-repl --debug-repl --interpreter r
```

Behavior:

- Enter multi-line input and finish it with a line ending in `END`
- Type `INTERRUPT` to send an interrupt
- Type `RESTART` to restart the worker
- Type `Ctrl-D` to exit

Useful environment variables:

- `MCP_REPL_IMAGES=0|1|kitty` controls inline image rendering in the debug REPL
- `MCP_REPL_PAGER_PAGE_CHARS=<n>` overrides the pager page size if you want larger or smaller pages while debugging

## External wire trace proxy

The built-in event log only sees what reaches `mcp-repl` after startup. If you need the exact stdio traffic between an MCP client and the server, use the external proxy in [scripts/mcp-stdio-trace.py](/Users/tomasz/github/t-kalinowski/mcp-repl/scripts/mcp-stdio-trace.py).

What it does:

- Spawns the real stdio MCP server, typically `mcp-repl`
- Forwards client stdin to server stdin
- Forwards server stdout back to the client
- Captures server stderr into the trace log
- Writes both a raw JSONL log and an indented `.pretty.json` log under `.mcp-repl-trace/` in the current working directory

Each captured chunk includes:

- Timestamp and pid
- Stream name and route
- Raw bytes as base64
- UTF-8 text when the chunk decodes cleanly
- Parsed JSON in `text_as_json` when the chunk is line-delimited JSON

Set `MCP_REPL_TRACE_FORWARD_STDERR=1` if you also want the proxied server `stderr` mirrored to your terminal. If `MCP_REPL_DEBUG_DIR` is set, the proxy writes `wire.jsonl` and `wire.pretty.json` into the same session directory and passes that directory to `mcp-repl`.

Direct invocation:

```sh
scripts/mcp-stdio-trace.py ~/.cargo/bin/mcp-repl --interpreter r
```

Client-config pattern:

```json
{
  "command": "/absolute/path/to/scripts/mcp-stdio-trace.py",
  "args": [
    "/absolute/path/to/mcp-repl",
    "--interpreter",
    "r",
    "--debug-dir",
    "/tmp/mcp-repl-debug"
  ]
}
```

That setup gives you two views at once:

- The proxy log shows the exact client/server traffic
- The session directory shows the internal `mcp-repl` interpretation of that traffic

## Claude clear-hook worktree

The `clean-claude-hook-session-reset` worktree adds Claude-specific debugging that is not on `main`.

Branch-specific surfaces:

- `mcp-repl claude-hook session-start`
- `mcp-repl claude-hook session-end`
- `CLAUDE_ENV_FILE`
- `MCP_REPL_CLAUDE_SESSION_ID`

That worktree also keeps inspectable Claude clear-hook state under:

- `$XDG_STATE_HOME/mcp-repl/claude-clear`
- `~/.local/state/mcp-repl/claude-clear` when `XDG_STATE_HOME` is not set

In that worktree, the JSONL debug log also records:

- `server_name` in the startup payload
- `claude_state_prune_begin`
- `claude_state_prune_end`

Use those only when you are debugging the Claude `/clear` reset flow on that branch.
