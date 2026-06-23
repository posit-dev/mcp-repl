---
title: Announcing mcp-repl
description: >
  A private, sandboxed R and Python runtime for AI agents
---

## What it is

`mcp-repl` is an open source MCP server that gives an AI agent a
private, persistent R or Python session.

It plugs into MCP-capable agents and gives them a live runtime:
persistent state, in-band help, plot capture, debugger support,
large-output guardrails, and sandboxed execution.

The session is agent-facing, not human-facing. It is private to the
model, persists across tool calls, and can be reset when the task should
start cleanly.

## The problem

R and Python are not only batch execution environments. They are
interactive environments.

Working effectively with R or Python often means keeping a live session
open: loading data, inspecting objects, reading help, making plots,
stepping through code, and iterating from the current state.

Many agents do not get that. They get shell commands:

```sh
Rscript -e '...'
python -c '...'
```

That works for one-off execution, but it is a poor fit for data work.
Loading data can be expensive. Objects in memory often become the
context for the next step. Plots need to be seen, not just saved.
Debugging often depends on interacting with a running session.

A stateless shell command throws that context away after every call.

Another approach is to give the agent a terminal session, often through
tools like `tmux`. That gives persistence, but usually introduces
polling: capture the terminal, inspect output, wait, capture again, and
guess whether the runtime is ready. That wastes context and makes
interactive work fragile.

`mcp-repl` is built around a different idea: if R and Python are
interactive environments for humans, agents need access to those same
affordances in a form they can use.

## The motivation

The goal of `mcp-repl` is to make R and Python more effective
environments for LLMs.

That means more than executing code. It means exposing the useful parts
of the interactive workflow: persistent objects, help systems, plots,
debugging, readline-style interaction, bounded output, and responsive
handling of both quick probes and long-running computations.

The shape is different for an LLM than for a human. A human benefits
from tab completion, scrollback, and a visible console. A model benefits
from compact help, structured output, image responses, explicit reset
controls, and a runtime that can report when it is actually idle.

`mcp-repl` adapts the interactive strengths of R and Python for agents
without replacing the underlying programming model.

## How it works

`mcp-repl` runs R or Python as a long-lived worker behind an MCP
interface.

The agent sends code through a `repl` tool. The session keeps its state
across calls. The agent can load data once, inspect objects, generate
plots, read documentation, debug, and continue from the same runtime.

The worker is sandboxed by default. Network access is disabled unless
configured, and writes are constrained to the workspace and session temp
paths. The sandbox is enforced with OS-level primitives, not prompt
instructions.

R and Python run embedded in `mcp-repl`, so it does not rely on
prompt-string polling, or fixed sleeps, or output timing heuristics, to
guess whether the interpreter is ready. It owns enough of the REPL loop
to handle interactive and long-running work more directly.

## What it is good for

`mcp-repl` is most useful when you want an agent to do data work without
watching every step. It gives the agent a private R or Python runtime
with persistent state, model-oriented output, plot support, and
sandboxed execution.

That makes it a good fit for unattended or lightly supervised workflows,
where you can launch an agent and come back hours later, such as:

- producing recurring reports with LLM assistance, such as “Analyze last
  week’s sales data, find what changed, and draft a report that
  highlights fresh, surprising, or concerning trends”
- evaluating agent capability on data-analysis tasks, such as conducting
  evals with tools like [Inspect](https://inspect.aisi.org.uk)
- commissioning initial reconnaissance work, such as “Explore this
  dataset, check data quality, identify the strongest signals, and
  suggest the next analyses worth running”
- debugging R or Python projects autonomously, such as “Reproduce this
  failing package example, inspect the live objects, step through the
  debugger, and propose a minimal fix”
- preparing artifacts for human review, such as “Iterate privately on
  the analysis, plots, and summary tables, then return the final result
  with the key decisions and caveats”

The sandbox matters for these workflows. Because the runtime is private
to the agent and may be used unattended, `mcp-repl` gives the model
useful R or Python capabilities without making the runtime equivalent to
an unrestricted shell.

`mcp-repl` is also useful in general-purpose agent harnesses.
MCP-capable tools such as Claude Code and Codex are not primarily built
for data analysis, but they are often used on R and Python projects, and
can be asked to perform tasks with data. Adding `mcp-repl` gives those
agents a live, persistent runtime and workbench instead of only isolated
shell commands.

For a fully integrated, human-in-the-loop data analysis product, Posit
Assistant goes further by combining a human-facing development
environment with agent-facing execution support. `mcp-repl` is narrower:
it helps autonomous agents and general-purpose agent harnesses do R and
Python data work more effectively.

## Why it is good for agents

`mcp-repl` is designed to give models the affordances they need: context
efficiency, token efficiency, reliable completion, and useful output.

Some of that work is intentionally below the surface:

- Session state persists across calls, reducing repeated setup.
- Data can stay loaded in memory across many steps.
- Output is curated so the model sees useful results without unnecessary
  noise.
- Smart echo behavior avoids repeating code when it does not help.
- Plot images are returned through MCP, so vision-capable models can
  inspect them directly.
- Large outputs stay bounded and can be saved into structured bundles
  with transcripts and plot files.
- R help, vignettes, and manuals render in-band.
- Python help through `help()`, `dir()`, and `pydoc` works inside the
  session.
- Interactive debugging flows, including R `browser()` and Python `pdb`,
  can be driven through the REPL.
- Explicit interrupt and reset controls give the agent a clear way to
  recover.

The goal is not to invent a new way to write R or Python. The goal is to
preserve the familiar runtime while shaping it for an LLM.

## How it relates to Posit Assistant

`mcp-repl` and Posit Assistant bring interactive data environments to
LLMs in different ways.

`mcp-repl` is a plug-in front end for agents. It works through MCP and
gives an agent a private R or Python runtime. That makes it useful in
existing agent harnesses and autonomous workflows.

Posit Assistant is an integrated experience for human-in-the-loop data
analysis. It combines a human-facing IDE with agent-facing execution
support, so the user and the model can work with shared project context.

Both are about making R and Python better environments for AI-assisted
data work. `mcp-repl` focuses on autonomous work in a private runtime.
Posit Assistant focuses on close collaboration between a human and a
model.

## Getting started

Install with the shell script on macOS or Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.sh | sh
```

On Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.ps1 | iex
Install-McpRepl
```

You can also install from source with Cargo:

```sh
cargo install --git https://github.com/posit-dev/mcp-repl --locked
```

The binaries do not bundle R or Python, so install those separately.

Then add `mcp-repl` to your MCP client configuration:

```sh
mcp-repl install
```

By default, this writes entries for both R and Python for supported
clients. You can also install only one interpreter:

```sh
mcp-repl install --interpreter r
mcp-repl install --interpreter python
```

Once configured, the MCP client exposes two tools:

- `repl`, for running code in the persistent session
- `repl_reset`, for starting over with a fresh session

## Open source

`mcp-repl` is open source under the Apache-2.0 license.

Its goal is narrow: make R and Python more capable environments for AI
agents, especially when those agents are running unattended.

Project repository:

https://github.com/posit-dev/mcp-repl
