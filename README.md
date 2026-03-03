# mcp-repl

`mcp-repl` is an MCP server that exposes a long-lived interactive REPL runtime over stdio.

It is backend-agnostic in design. The default interpreter is R, with an opt-in Python interpreter (`--interpreter python`).

Session state persists across calls, so agents can iterate in place, inspect intermediate values, debug, and read docs in-band.

## Why use it

- Stateful REPL execution in one long-lived process.
- LLM-oriented output handling: prompt/echo cleanup and built-in pager mode.
- In-band docs for common help flows (`?`, `help()`, `vignette()`, `RShowDoc()`).
- Plot images returned as MCP image content.
- OS-level sandboxing by default, plus a memory resource guardrail.

### Safe by default

Like a shell, R and Python are powerful. Without guardrails, an LLM can do real damage on the host (both accidental and prompt-induced). To reduce this risk, `mcp-repl` runs the backend process in a sandboxed environment. By default, network is disabled and writes are constrained to workspace roots and temp paths required by the worker. Sandbox policy is enforced with OS primitives at the process level, not command-specific runtime rules. On Unix backends, `mcp-repl` also enforces a memory resource guardrail on the child process tree and kills the worker if it exceeds the configured threshold.

### Token efficient

`mcp-repl` can be substantially more token efficient for an LLM than a standard persistent shell call. It includes affordances tailored to common LLM workflow strengths and weaknesses. For example:
- There is rarely a need to repeatedly poll, since the console is embedded in the backend and normally returns as soon as evaluation is complete.
- Echoed inputs are automatically pruned or elided so output is easy to attribute.
- A rich pager, purpose-built for an LLM, prevents context floods while supporting search and controlled navigation.
- Documentation receives special handling. Built-in entry points like `?`, `help`, `vignette()`, and `RShowDoc()` are customized to present plain text or converted Markdown in-band, replacing the usual HTML browser flow.

### Pager

The pager activates only when output exceeds roughly one page, and scales from small multi-page outputs to hundreds of pages (for example, navigating the R manuals). It is designed to keep context focused for the model while still allowing deterministic navigation.

Internally, the pager is backed by a bounded ring buffer with an event timeline, not a naive "dump and slice" stream. That gives it predictable memory usage while still supporting strong navigation semantics:
- Output is tracked with stable offsets, so commands like `:seek` (offset/percent/line) and `:range` can jump deterministically.
- Text and image events are merged into one timeline, so pagination decisions can account for both without duplicating content.
- Already-shown ranges and images are tracked explicitly; when overlap occurs, the pager emits offset-based elision markers instead of replaying content.
- UTF-8-aware indexing keeps search and cursor movement aligned to characters while preserving exact byte offsets internally.

These affordances are all driven by observed LLM workflows and aim to reduce token waste while improving access to reference material.

### Plots

`mcp-repl` provides a private space for the LLM to easily visualize plots of data. This allows it to iterate safely and privately, without demanding your attention until it can return with a grounded, verified result.

## Quickstart

### 1) Install

#### Prerequisite: Cargo

You need `cargo` (the Rust toolchain). The standard way to install it is `rustup`:

```sh
# See https://rustup.rs for details.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

#### Install with cargo (from GitHub)

```sh
cargo install --git https://github.com/t-kalinowski/mcp-repl --locked
```

This installs `mcp-repl` into Cargo’s bin directory (typically `~/.cargo/bin`). Ensure that directory is on your `PATH`.

### 2) Wire into your MCP client

Point your MCP client at the binary (either via `PATH` or by using an explicit path like `~/.cargo/bin/mcp-repl` or `target/release/mcp-repl`).

You can auto-install into existing agent config files:

```sh
# install to all existing agent homes (does not create ~/.codex or ~/.claude)
mcp-repl install

# install only codex MCP config
mcp-repl install --client codex

# install only claude MCP config
# Note: there may be some rough edges with Claude.
# This has been primarily developed and tested with Codex.
mcp-repl install --client claude

# install only one interpreter for a specific client
mcp-repl install --client codex --interpreter r
```

`install --client codex` writes `--sandbox inherit` by default. That sentinel means `mcp-repl` should
inherit sandbox policy updates from Codex for the session.

Example `R` REPL Codex config (paths vary by OS/user):

```toml
[mcp_servers.r_repl]
command = "/Users/alice/.cargo/bin/mcp-repl"
# mcp-repl handles the primary timeout; this higher Codex timeout is only an outer guard.
tool_timeout_sec = 1800
# --sandbox inherit: use sandbox policy updates sent by Codex for this session.
# If no update is sent, mcp-repl exits with an error.
args = [
  "--sandbox", "inherit",
  "--interpreter", "r",
]
```

Example `Python` REPL Codex config:

```toml
[mcp_servers.py_repl]
command = "/Users/alice/.cargo/bin/mcp-repl"
# mcp-repl handles the primary timeout; this higher Codex timeout is only an outer guard.
tool_timeout_sec = 1800
# --sandbox inherit: use sandbox policy updates sent by Codex for this session.
# If no update is sent, mcp-repl exits with an error.
args = [
  "--sandbox", "inherit",
  "--interpreter", "python",
]
```

For Claude, `install --client claude` writes explicit sandbox mode by default because Claude does not
propagate sandbox state updates to MCP servers:

```json
{
  "mcpServers": {
    "r_repl": {
      "command": "/Users/alice/.cargo/bin/mcp-repl",
      "args": ["--sandbox", "workspace-write", "--interpreter", "r"]
    },
    "py_repl": {
      "command": "/Users/alice/.cargo/bin/mcp-repl",
      "args": ["--sandbox", "workspace-write", "--interpreter", "python"]
    }
  }
}
```

By default install creates one entry per supported interpreter:
- `r_repl`
- `py_repl`

Use `--interpreter r`, `--interpreter python`, or comma-separated/repeatable forms
to limit which interpreters are installed.

Optional: enable rich JSONL debug logs for each `mcp-repl` startup:

- CLI arg: `--debug-events-dir /path/to/log-dir`
- Environment: `MCP_REPL_DEBUG_EVENTS_DIR=/path/to/log-dir`

When enabled, each startup writes a new `mcp-repl-*.jsonl` file containing startup
metadata (cwd, argv, Codex session hints) plus tool calls and sandbox state updates.

### 3) Pick interpreter (optional)

- Default interpreter: R
- CLI: `mcp-repl --interpreter r|python`
- Environment: `MCP_REPL_INTERPRETER=r|python` (compatibility alias: `MCP_REPL_BACKEND`)

## Runtime discovery

### Interpreter selection order

`mcp-repl` chooses interpreter in this order:
- `--interpreter <r|python>` (if provided)
- compatibility CLI alias: `--backend <r|python>`
- `MCP_REPL_INTERPRETER`
- compatibility env alias: `MCP_REPL_BACKEND`
- default: `r`

### R interpreter: which R installation is used

- To force a specific R installation, set `R_HOME` in the environment that launches `mcp-repl`.
- If `R_HOME` is not set, `mcp-repl` discovers it from `R` on `PATH` (via `R RHOME`).
- To verify which R is active, run `R.home()` in the console session.

### Python interpreter: which Python installation is used

Interpreter resolution order:
- nearest `.venv/bin/python` from current working directory upward
- nearest `.venv/bin/python3` from current working directory upward
- first executable `python3` on `PATH`
- first executable `python` on `PATH`
- fallback literal `python3`

Notes:
- Upward `.venv` search stops at `$HOME` (inclusive) when applicable, otherwise at filesystem root.
- Python backend starts in basic REPL mode (`PYTHON_BASIC_REPL=1`) and loads `python/driver.py`.

## Platform support

- **macOS / Linux**: supported.
- **Windows**: experimental for the R backend. The Python backend currently requires a Unix PTY and is not available on Windows.

## Sandbox

Default sandbox policy is `workspace-write` with network disabled.
Write access includes the working area and temp paths required by the worker (exact roots vary by OS/policy).
On Windows, sandbox enforcement is still under active development and is not yet fully functional/reliable across environments.

See `docs/sandbox.md` for precise behavior, runtime updates, and OS-specific details.

## MCP surface

Primary REPL-aligned tools:
- `repl` -> `{ "input": "1+1\n", "timeout_ms": 10000 }`
- `repl_reset` -> `{}`

Tool guides:
- `docs/tool-descriptions/repl_tool_r.md`
- `docs/tool-descriptions/repl_tool_python.md`
- `docs/tool-descriptions/repl_reset_tool.md`

## Session management

- **Interrupt**: prefix `repl` input with `\u0003` (best-effort SIGINT). If successful, the same session continues.
- **Reset**: call `repl_reset`, or prefix `repl` input with `\u0004` (Ctrl-D). With `\u0004`, remaining input (optional newline) is executed in the fresh session.
- **Reset escalation model**: reset first attempts graceful session shutdown, then escalates to forceful termination; on Unix, if process-group signaling is unavailable, it falls back to scanning and signaling descendant processes.
- **In-band exits**: standard runtime exits also work (`EOF`, `quit()`, etc.); output is returned and the next request runs in a fresh worker.

## Docs

Tool behavior and usage guidance:
- `docs/tool-descriptions/repl_tool_r.md`
- `docs/tool-descriptions/repl_tool_python.md`
- `docs/tool-descriptions/repl_reset_tool.md`

Additional references:
- Sandbox behavior and configuration: `docs/sandbox.md`
- Worker sideband protocol: `docs/worker_sideband_protocol.md`

## License

Apache-2.0. See `LICENSE`.
