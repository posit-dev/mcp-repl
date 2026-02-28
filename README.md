# mcp-repl

`mcp-repl` is an MCP server that exposes a long-lived interactive REPL runtime over stdio.

It is designed to accelerate LLMs at tasks like EDA (Exploratory data analysis) and debugging of interperted code (like R or Python). The REPL gives the LLM a tight feed-back loop for exploration and development, just like it does for a human.

Unlike a regular REPL, `mcp-repl` presents an interface that is tailored to the
strengths and weaknesses of an LLM. It is chock-full of affordances designed for agents, giving them access to the same affordances as a repl does a human, but with much more token efficient way, and with the safety of a process sandbox.


It is backend-agnostic in design. It comes with built in support for R and Python.


## Highlights:

### Safe by default

The interperter runs in a sandbox. These are restrictions on the process built with OS primitives, not command-specific runtime rules.
By default, the process only has permissions to write to the current in the current directory, and network is disabled (many system calls generally are disabled).

It is also possible to configure the sandbox policy, with extra affordances for adding additional writeable directories, or allowing a specific set of network domains that can be accessed. On Unix backends, `mcp-repl` also enforces a memory resource guardrail on the child process tree and kills the worker if it exceeds the configured threshold.


### Plots

`mcp-repl` provides a private space for the LLM to easily visualize plots of data. This allows it to iterate safely and privately, without demanding your attention until it can return with a grounded, verified result.


### Token efficient

`mcp-repl` can be substantially more token efficient for an LLM than a standard persistent shell call. It includes affordances tailored to common LLM workflow strengths and weaknesses. For example:
- There is rarely a need for the LLM to poll the pty, since the console is embedded in the backend and returns normally only when evaluation is complete.
- Echoed inputs are automatically pruned or elided to save context, but in a way where output is always easy to attribute to individual commands.

- Documentation receives special handling. Built-in entry points like `?`, `help()`, `vignette()`, and `RShowDoc()` are customized to present plain text or converted Markdown in-band.
- A rich pager, purpose-built for an LLM, prevents context floods while supporting search and controlled navigation.




#### Pager

The pager activates only when output exceeds roughly one page, and scales from small multi-page outputs to hundreds of pages (for example, navigating the R manuals). It is designed to keep context focused for the model while still allowing deterministic navigation.

Internally, the pager is backed by a bounded ring buffer with an event timeline That gives it predictable memory usage and strong navigation semantics.


The llm can use the pager to `:seek` or jump to a `:range` with (offset/percent/line) values. If the llm jumps around, the pager _never_ shows duplicated content - instead inserts a reference back to the earlier shown content. This enalbes the llm to efficiently browse large documents without wasting context on repeated content.

Already-shown ranges and images are tracked explicitly; when overlap occurs, the pager emits offset-based elision markers instead of replaying content.

Text and image events are merged into one timeline, so pagination decisions can account for both without duplicating content.

These affordances are all driven by observed LLM workflows and aim to reduce token waste while improving access to reference material.



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

You can auto-install into existing agent config files both `R` and `python` tools:

```sh
# install to all existing agent homes (does not create ~/.codex or ~/.claude)
mcp-repl install
```

If you only want to install one interperter tool, or only for a specific client, you can specify it:

```sh
mcp-repl install --client codex --interpreter r
```


`install --client codex` writes `--sandbox inherit` by default. That sentinel means `mcp-repl` should
inherit sandbox policy updates from Codex for the session.

For `r_repl`, install now also tries to discover the R cache root at install time and appends it
as an extra writable root. This uses:
- `dirname(tools::R_user_dir("mcp_repl_install_probe", which = "cache"))`
- If the probe fails, no extra writable root is added.

Example `R` REPL Codex config (paths vary by OS/user and platform):

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
  "--add-writable-root", "/Users/alice/Library/Caches/org.R-project.R/R",
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
      "args": ["--sandbox", "workspace-write", "--interpreter", "r", "--add-writable-root", "/Users/alice/Library/Caches/org.R-project.R/R"]
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

To include additional domain allowlist entries in generated client config, pass repeatable install args:

```sh
mcp-repl install --client codex --interpreter python \
  --arg=--sandbox --arg=workspace-write \
  --arg=--config --arg='sandbox_workspace_write.network_access=true' \
  --arg=--add-allowed-domain --arg=pypi.org \
  --arg=--add-allowed-domain --arg=files.pythonhosted.org
```

Equivalent manual `~/.codex/config.toml` example:

```toml
[mcp_servers.py_repl]
command = "/Users/alice/.cargo/bin/mcp-repl"
tool_timeout_sec = 1800
args = [
  "--sandbox", "workspace-write",
  "--config", "sandbox_workspace_write.network_access=true",
  "--add-allowed-domain", "pypi.org",
  "--add-allowed-domain", "files.pythonhosted.org",
  "--interpreter", "python",
]
```

## Runtime discovery

### Interpreter selection order

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

Notes:
- Upward `.venv` search stops at `$HOME` (inclusive) when applicable, otherwise at filesystem root.


## Sandbox

Default sandbox policy is `workspace-write` with network disabled.
Write access includes the working area and temp paths required by the worker (exact roots vary by OS/policy).
On Windows, sandbox enforcement is still under active development and is not yet fully functional/reliable across environments.

See:
- `docs/sandbox.md` for a quick reference.
- `docs/sandbox-config.md` for detailed semantics, preset behavior, platform-specific behavior, and allowed-domain interactions.

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
- Detailed sandbox semantics and platform matrix: `docs/sandbox-config.md`
- Worker sideband protocol: `docs/worker_sideband_protocol.md`

## License

Apache-2.0. See `LICENSE`.
