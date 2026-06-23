---
title: Announcing mcp-repl
description: >
  A sandboxed R and Python REPL for MCP-capable agents
---

## What it is

`mcp-repl` is an open source MCP server that gives AI agents an R or
Python REPL.

It is built for model-facing workflows rather than human-facing
consoles. The session keeps state across tool calls, returns plots
through MCP, renders help in-band, supports debugger interaction, bounds
large outputs, and provides explicit interrupt and reset controls.

The goal is narrow: give agents the interactive affordances that make R
and Python useful for real data work, without turning the runtime into
an unrestricted shell.

## Why agents need more than shell commands

R and Python are often used interactively. A useful session accumulates
context: objects, plots, package state, documentation lookups, warnings,
errors, and debugging frames.

Many agents instead get batch commands:

```sh
Rscript -e '...'
python -c '...'
```

That is fine for isolated probes. It is a poor fit for exploratory
analysis, project debugging, and long-running work. Each command starts
over, so the agent has to reconstruct state instead of continuing from
it.

A terminal session can preserve state, but usually leaves the agent with
an unstructured stream of text. The agent may have to poll for output,
infer whether the interpreter is ready, and guess how to handle prompts,
plots, pagers, and debuggers.

`mcp-repl` provides a structured REPL interface for this kind of work.

## A typical agent workflow

An agent using `mcp-repl` can move through an analysis in small steps
without restarting the runtime each time.

For example, it might inspect a dataset, check data quality, create
summary tables, generate plots, read documentation for an unfamiliar
function, fit a model, enter the debugger after an error, inspect live
objects, revise the code, and return a final report with caveats.

[Optional: replace this paragraph with a concrete demo, including the dataset, prompt, and one or two representative outputs.]

## How it works

`mcp-repl` runs R or Python as a long-lived worker behind an MCP
interface.

The agent sends code through a `repl` tool. The worker evaluates it,
captures useful output, and reports when the interpreter is ready for
the next step. Because `mcp-repl` owns the REPL loop, it does not need
prompt-string polling, fixed sleeps, or output-timing heuristics.

The worker is sandboxed by default. Network access is disabled unless
configured, and writes are constrained to the workspace and session
temporary paths. The sandbox is enforced with OS-level primitives, not
prompt instructions.

[TODO: Add precise platform details for the sandboxing model.]

[TODO: Link to sandboxing documentation and any known limitations.]

## What the agent gets

`mcp-repl` exposes the parts of R and Python that matter during
interactive work:

- stateful execution across tool calls
- bounded, model-oriented output
- plot capture through MCP
- R help, vignettes, and manuals in-band
- Python help through `help()`, `dir()`, and `pydoc`
- support for R `browser()` and Python `pdb`
- transcript and plot bundles for large results
- interrupt and reset controls for recovery

These features are not a new programming model. They are the existing R
and Python workflow adapted to an agent interface.

## Where it fits

`mcp-repl` is useful when an MCP-capable agent needs to do R or Python
work with less supervision.

Common uses include:

- exploratory data analysis
- recurring reports
- project debugging
- agent evals involving data-analysis tasks
- general-purpose coding agents working on R or Python projects

Because the runtime may be used unattended, the sandbox is part of the
product rather than an optional layer around it.

## How it relates to Posit Assistant

`mcp-repl` and Posit Assistant address different parts of AI-assisted
data work.

`mcp-repl` is a plug-in runtime for autonomous or lightly supervised
agents. It gives existing MCP clients a sandboxed R or Python REPL.

Posit Assistant is an integrated, human-in-the-loop product. It combines
a development environment with agent-facing execution support, so the
user and model can work with shared project context.

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

[Optional: explain why the Windows install uses two commands.]

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

- `repl`, for running code in the session
- `repl_reset`, for starting over

[Optional: link to manual installation instructions for readers who prefer to inspect install scripts before running them.]

## Open source

`mcp-repl` is open source under the Apache-2.0 license.

Project repository:

https://github.com/posit-dev/mcp-repl
