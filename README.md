# mcp-repl

`mcp-repl` is an MCP server that provides a REPL for agents.

It gives an agent a persistent R or Python session that stays alive across tool calls, so it can work the way a person would in a REPL: load data once, inspect objects, try ideas, read help, make plots, and keep iterating in context.


## Why use it

A shell tool can run `Rscript -e` or `python -c`, but that is not the same as having a session.

Data analysis languages were designed with interactive affordances. To be able to take full advantage of what the runtime offers, it only makes sense for an LLM to also be able to access those same interactive workflows.

If the work is exploratory, stateful, or iterative, a throwaway command runner keeps forcing the agent to rebuild context. `mcp-repl` keeps the session open instead. That makes a difference for:

- data exploration
- interactive help and documentation lookup
- plotting and visual checks
- debugging
- any workflow where intermediate objects should stay in memory

It is built for real agent use: sandboxing is on by default, plots and help are supported in-band, session control with interrupts or restarts is explicit, and large replies stay readable.

## How it works

Your MCP client sends code to `repl`. `mcp-repl` runs it inside a long-lived R or Python process and keeps that process alive for the next call.

That means variables, loaded packages, imported modules, plots, and other session state remain available until you reset the session or exit it.

Results come back as text and, when relevant, images.

## What it is good at

- Exploring data without rebuilding context on every turn.
- Reading help in-band instead of bouncing out to a browser.
- Producing plots the agent can inspect immediately.
- Iterating in a private scratch session before returning an answer.
- Multi-step analysis where keeping state saves time and tokens.

### Safe by default

Like a shell, R and Python are powerful. Without guardrails, an LLM can do real damage on the host (both accidental and prompt-induced). To reduce this risk, `mcp-repl` runs the backend process in a sandboxed environment. By default, network is disabled and writes are constrained to workspace roots and temp paths required by the active R or Python session. Sandbox policy is enforced with OS primitives at the process level, not command-specific runtime rules. On Unix backends, `mcp-repl` also enforces a memory resource guardrail on the child process tree and kills the worker if it exceeds the configured threshold.

## Token efficient

### Keeps output readable

REPL output can get verbose and messy quickly. `mcp-repl` curates the response to avoid wasting tokens or confusing the model:

- Smart echo behavior: no echo when it is safe to omit, and elided or collapsed echo for large multi-expression blocks. Input is reflected only when needed to connect output back to the code that produced it.
- Help pages render in-band instead of opening a separate browser flow.
- Very large replies stay compact in the tool response, with a preview and a path to the full saved output when needed.
- Plot images are returned directly through MCP instead of requiring a separate GUI workflow.

### Large outputs still work

Most replies stay inline. When output gets too large, `mcp-repl` keeps the immediate response short and saves the full output as a structured bundle on disk.

Models are good at searching and exploring files when the structure is clear. Instead of flooding the tool reply, `mcp-repl` produces a bundle the model can inspect on demand: compact previews in the immediate response, plus stable paths to the full transcript and plot files when deeper exploration is needed.

The practical effect is simple:

- the tool reply stays readable
- the full output is still available
- the model can explore the saved bundle incrementally
- long transcripts and plot-heavy replies do not flood model context

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
cargo install --git https://github.com/posit-dev/mcp-repl --locked
```

To install a specific version, pin the tag:

```sh
cargo install --git https://github.com/posit-dev/mcp-repl --tag v0.1.0 --locked
```

This installs `mcp-repl` into Cargo’s bin directory (typically `~/.cargo/bin`). Ensure that directory is on your `PATH`.

#### Install prebuilt binaries

Stable installs use the latest non-prerelease GitHub Release. Dev installs use the rolling `dev` prerelease.

Linux / macOS stable:

```sh
curl -fsSL https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.sh | sh
```

Linux / macOS dev:

```sh
curl -fsSL https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.sh | sh -s -- --dev
```

Windows PowerShell stable:

```powershell
irm https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.ps1 | iex
Install-McpRepl
```

Windows PowerShell dev:

```powershell
irm https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.ps1 | iex
Install-McpRepl -Dev
```

#### Download prebuilt stable binaries

Stable release page:

- `https://github.com/posit-dev/mcp-repl/releases/latest`

Stable asset URLs:

- Linux x86_64 (glibc build produced on Ubuntu 22.04): `https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-x86_64-unknown-linux-gnu.tar.gz`
- macOS arm64: `https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-aarch64-apple-darwin.tar.gz`
- Windows x86_64 (experimental): `https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-x86_64-pc-windows-msvc.zip`

#### Download prebuilt dev binaries

The rolling `dev` prerelease publishes the newest available `main` build at stable URLs:

- Dev release page: `https://github.com/posit-dev/mcp-repl/releases/tag/dev`
- Linux x86_64 (glibc build produced on Ubuntu 22.04): `https://github.com/posit-dev/mcp-repl/releases/download/dev/mcp-repl-x86_64-unknown-linux-gnu.tar.gz`
- macOS arm64: `https://github.com/posit-dev/mcp-repl/releases/download/dev/mcp-repl-aarch64-apple-darwin.tar.gz`
- Windows x86_64 (experimental): `https://github.com/posit-dev/mcp-repl/releases/download/dev/mcp-repl-x86_64-pc-windows-msvc.zip`

These binaries do not bundle R or Python. You still need compatible local runtimes installed.
After download, unpack the archive and put `mcp-repl` (or `mcp-repl.exe` on Windows) on your `PATH`.

### 2) Wire into your MCP client

Point your MCP client at the binary (either via `PATH` or by using an explicit path like `~/.cargo/bin/mcp-repl` or `target/release/mcp-repl`).

You can auto-install into existing agent config files:

```sh
# bare mcp-repl defaults to pager unless you pass --oversized-output explicitly

# install to all available targets (does not create ~/.codex if missing)
mcp-repl install

# install only codex MCP config
mcp-repl install --client codex

# install only claude MCP config (writes to ~/.claude.json)
mcp-repl install --client claude

# install only one interpreter for a specific client
mcp-repl install --client codex --interpreter r
```

Bare `mcp-repl` defaults to `--oversized-output pager`.

`install --client codex` writes `--sandbox inherit --oversized-output files` by default. That
sentinel means `mcp-repl` should inherit sandbox policy updates from Codex for the session while
keeping installed Codex configs on the file-backed oversized-output path.

Example `R` REPL Codex config (paths vary by OS/user):

```toml
[mcp_servers.r]
command = "/Users/alice/.cargo/bin/mcp-repl"
# mcp-repl handles the primary timeout; this higher Codex timeout is only an outer guard.
tool_timeout_sec = 1800
# --sandbox inherit: use sandbox policy updates sent by Codex for this session.
# If no update is sent, mcp-repl exits with an error.
args = [
  "--sandbox", "inherit",
  "--oversized-output", "files",
  "--interpreter", "r",
]
```

Example `Python` REPL Codex config:

```toml
[mcp_servers.python]
command = "/Users/alice/.cargo/bin/mcp-repl"
# mcp-repl handles the primary timeout; this higher Codex timeout is only an outer guard.
tool_timeout_sec = 1800
# --sandbox inherit: use sandbox policy updates sent by Codex for this session.
# If no update is sent, mcp-repl exits with an error.
args = [
  "--sandbox", "inherit",
  "--oversized-output", "files",
  "--interpreter", "python",
]
```

For Claude, `install --client claude` writes to `~/.claude.json` with explicit sandbox mode and
`--oversized-output files` because Claude does not propagate sandbox state updates to MCP servers:

```json
// ~/.claude.json
{
  "mcpServers": {
    "r": {
      "command": "/Users/alice/.cargo/bin/mcp-repl",
      "args": ["--sandbox", "workspace-write", "--oversized-output", "files", "--interpreter", "r"]
    },
    "python": {
      "command": "/Users/alice/.cargo/bin/mcp-repl",
      "args": ["--sandbox", "workspace-write", "--oversized-output", "files", "--interpreter", "python"]
    }
  }
}
```

By default install creates one entry per supported interpreter:
- `r`
- `python`

Use `--interpreter r`, `--interpreter python`, or comma-separated/repeatable forms
to limit which interpreters are installed.

Optional: enable rich JSONL debug logs for each `mcp-repl` startup:

- CLI arg: `--debug-dir /path/to/debug-root`
- Environment: `MCP_REPL_DEBUG_DIR=/path/to/debug-root`

When enabled, each startup writes a new session directory under that root containing
`events.jsonl`, startup logs, and sandbox-state logs.

See [docs/debugging.md](docs/debugging.md) for the full debugging guide, including
startup logs, sandbox-state tracing, and the external wire-trace proxy.

### 3) Pick interpreter (optional)

- Default interpreter: R
- CLI: `mcp-repl --interpreter r|python`
- Environment: `MCP_REPL_INTERPRETER=r|python`

## Runtime discovery

### Interpreter selection order

`mcp-repl` chooses interpreter in this order:
- `--interpreter <r|python>` (if provided)
- `MCP_REPL_INTERPRETER`
- default: `r`

### R interpreter: which R installation is used

- To force a specific R installation, set `R_HOME` in the environment that launches `mcp-repl`.
- If `R_HOME` is not set, `mcp-repl` discovers it from `R` on `PATH` (via `R RHOME`).
- To verify which R is active, run `R.home()` in the REPL session.

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

- **macOS**: supported.
- **Linux**: supported. Dev binaries are a glibc build produced on Ubuntu 22.04.
- **Windows**: experimental for the R backend. The Python backend currently requires a Unix PTY and is not available on Windows.

## Sandbox

Default sandbox policy is `workspace-write` with network disabled.
Write access includes the working area and temp paths required by the worker (exact roots vary by OS/policy).
On Windows, the experimental R sandbox uses parent-prepared workspace ACL state plus launch-scoped session-temp ACLs. Some environments still reject the restricted-token setup required by sandboxed modes.

See `docs/sandbox.md` for precise behavior, runtime updates, and OS-specific details.

## MCP surface

Primary REPL-aligned tools:
- `repl` -> `{ "input": "1+1\n", "timeout_ms": 10000 }`
- `repl_reset` -> `{}`

The exact `repl` tool description selected at startup depends on the interpreter and
`--oversized-output` mode.

Tool guides:
- `docs/tool-descriptions/repl_tool_r.md`
- `docs/tool-descriptions/repl_tool_r_pager.md`
- `docs/tool-descriptions/repl_tool_python.md`
- `docs/tool-descriptions/repl_tool_python_pager.md`
- `docs/tool-descriptions/repl_reset_tool.md`

## Session management

- **Interrupt**: prefix `repl` input with `\u0003` (best-effort SIGINT). If successful, the same session continues.
- **Reset**: call `repl_reset`, or prefix `repl` input with `\u0004` (Ctrl-D). With `\u0004`, remaining input (optional newline) is executed in the fresh session.
- **Reset escalation model**: reset first attempts graceful session shutdown, then escalates to forceful termination; on Unix, if process-group signaling is unavailable, it falls back to scanning and signaling descendant processes.
- **In-band exits**: standard runtime exits also work (`EOF`, `quit()`, etc.); output is returned and the next request runs in a fresh worker.

## Docs

Start with `docs/index.md` if you want the engineering map for the repository.

Tool behavior and usage guidance:
- `docs/tool-descriptions/repl_tool_r.md`
- `docs/tool-descriptions/repl_tool_python.md`
- `docs/tool-descriptions/repl_reset_tool.md`

Additional references:
- Debugging and tracing: `docs/debugging.md`
- Sandbox behavior and configuration: `docs/sandbox.md`
- Worker sideband protocol: `docs/worker_sideband_protocol.md`

## License

Apache-2.0. See `LICENSE`.
