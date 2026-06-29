# mcp-repl

`mcp-repl` is an MCP server that gives an agent a persistent Python or R
session, kept alive across tool calls. The agent can load data once,
inspect objects, try ideas, read help, make plots, and keep iterating —
the way a person would in a REPL.

A shell tool running `Rscript -e` or `python -c` keeps forcing the agent
to rebuild context. `mcp-repl` keeps the session open instead:
variables, loaded packages, plots, and other state stay available until
you or the model reset.

## Features

- **Sandboxed by default.** The backend runs in a sandbox enforced by OS
  primitives at the process level — not command-specific runtime rules.
  Network is disabled; writes are constrained to workspace roots and the
  temp paths the active session needs. On Unix, a memory guardrail kills
  the worker if it exceeds threshold.
- **Curated output.** Smart echo (omitted when safe, elided for large
  multi-expression blocks) and in-band help pages. Plots are returned as
  inline images through MCP for vision-capable models, so the agent sees
  the plot directly; non-vision models still get the saved file path.
  When replies get too large, the tool response stays short and the full
  output is saved as a structured bundle (transcript + plot files) the
  model can explore on demand.
- **No polling.** R and Python run embedded in the worker, not behind a
  stdio pipe driven by prompt-string heuristics. The server knows
  precisely when the interpreter is idle and has settled, so each `repl`
  call returns the moment the work is done — no fixed waits, no guessing
  whether more output is on the way.
- **Explicit session control.** Interrupts and resets are first-class.

## Quickstart

### 1. Install

Install from PyPI. The package is named `posit-mcp-repl` and exposes the
`mcp-repl` executable, plus a `posit-mcp-repl` alias for `uvx`:

```sh
pipx install posit-mcp-repl
# or
uv tool install posit-mcp-repl
# one-off
uvx posit-mcp-repl --help
```

Or install via `cargo` (needs the [Rust toolchain](https://rustup.rs)):

```sh
cargo install --git https://github.com/posit-dev/mcp-repl --locked
# pin a version with: --tag v0.1.0
```

Or use a prebuilt binary. Linux/macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.ps1 | iex
Install-McpRepl
```

Direct downloads live on the [latest release page](
  https://github.com/posit-dev/mcp-repl/releases/latest
). Linux x86_64 builds require glibc 2.35+; the glibc build produced on
Ubuntu 22.04 supports Ubuntu 22.04+.

Latest release binaries:

- Linux x86_64:
  https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-x86_64-unknown-linux-gnu.tar.gz
- macOS arm64:
  https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-aarch64-apple-darwin.tar.gz
- Windows x86_64:
  https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-x86_64-pc-windows-msvc.zip

PyPI wheels and prebuilt binaries do not bundle R or Python; install
those separately.

### 2. Wire into your MCP client

Auto-install into agent config files:

```sh
mcp-repl install                                 # all supported clients/interpreters
mcp-repl install --client codex                  # only Codex
mcp-repl install --client claude                 # only Claude (~/.claude.json)
mcp-repl install --client codex --interpreter r  # limit to one interpreter
```

By default this writes entries for both `r` and `python`.

`install --client codex` writes
`--sandbox inherit-codex --oversized-output files` — `inherit-codex` tells
`mcp-repl` to take sandbox policy from Codex's per-call
`_meta["codex/sandbox-state-meta"]` metadata (it fails closed if the metadata
is missing or malformed).
`install --client claude` writes an explicit `--sandbox workspace-write`
because Claude Code does not provide a way to propagate Codex's per-call
sandbox metadata to MCP servers. Bare `mcp-repl` (no install) defaults to
`--oversized-output pager`.

Manual Codex entry:

```toml
[mcp_servers.r]
command = "/Users/alice/.cargo/bin/mcp-repl"
tool_timeout_sec = 1800   # outer guard; mcp-repl handles the primary timeout
args = ["--sandbox", "inherit-codex", "--oversized-output", "files", "--interpreter", "r"]
```

Swap `--interpreter r` for `--interpreter python` (and rename the
section) for the Python entry.
Existing Codex configs that use `--sandbox inherit` still work; it is a
compatibility alias for `inherit-codex`.

Manual Claude entry in `~/.claude.json`:

```json fmt:skip
{
  "mcpServers": {
    "r": {
      "command": "/Users/alice/.cargo/bin/mcp-repl",
      "args": [
        "--sandbox", "workspace-write",
        "--oversized-output", "files",
        "--interpreter", "r"
      ]
    }
  }
}
```

### 3. Pick interpreter (optional)

Resolution order: `--interpreter <r|python>` → `MCP_REPL_INTERPRETER` →
`r`.

## Runtime discovery

**R.** Set `R_HOME` to force a specific installation; otherwise it's
discovered from `R` on `PATH` (via `R RHOME`). Verify with `R.home()` in
the session.

**Python.** The interpreter resolves in this order:

- nearest `.venv/bin/python` walking upward from cwd
- nearest `.venv/bin/python3` walking upward from cwd
- first `python3` on `PATH`
- first `python` on `PATH`
- fallback literal `python3`

`.venv` search stops at `$HOME` (inclusive), otherwise at the filesystem
root. The selected Python must expose a loadable CPython library via its
`sysconfig` metadata. Runtime-owned stdout/stderr is routed through
worker IPC; raw fd writes and child-process output are still captured
from the worker's stdout/stderr pipes.

## Platform support

- **macOS**: supported.
- **Linux**: supported. Release binaries are glibc builds produced on
  Ubuntu 22.04.
- **Windows**: experimental for R. Python is not part of the stable
  Windows surface yet.

## Sandbox

Default policy: `workspace-write` with network disabled. Write access
covers the working area plus worker-required temp paths (exact roots
vary by OS/policy). On Windows, the experimental R sandbox uses
parent-prepared workspace ACLs plus launch-scoped session-temp ACLs;
some environments reject the restricted-token setup.

See `docs/sandbox.md` for precise behavior.

## MCP surface

- `repl` → `{ "input": "1+1\n", "timeout_ms": 10000 }`

The exact `repl` tool description depends on the interpreter and
`--oversized-output` mode. Per-tool guides live in
`docs/tool-descriptions/`.

### Session control

- **Interrupt**: prefix `repl` input with `\u0003` (SIGINT, best-effort). Session continues.
- **Reset**: prefix `repl` input with `\u0004` (Ctrl-D / EOF). Reset
  requests worker shutdown, waits through a bounded graceful shutdown window,
  escalates to forceful termination when that window expires, then starts a
  fresh session. The same reply includes old-worker output captured through
  that window, followed by any remaining input's fresh-session output under the
  original call timeout.
- **In-band exits**: `EOF`, `quit()`, etc. also work — output is
  returned and the next request runs in a fresh worker.

## Debugging

Enable JSONL logs per startup:

- CLI: `--debug-dir /path/to/debug-root`
- Env: `MCP_REPL_DEBUG_DIR=/path/to/debug-root`

Each startup writes a session directory with `events.jsonl`, startup
logs, and sandbox-state logs. See [`docs/debugging.md`](
  docs/debugging.md) for the full guide, including the external
wire-trace proxy.

## Docs

- Engineering map: `docs/index.md`
- Sandbox: `docs/sandbox.md`
- Worker sideband protocol: `docs/worker_sideband_protocol.md`
- Tool guides: `docs/tool-descriptions/`

## License

Apache-2.0. See `LICENSE`.
