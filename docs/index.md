# Docs Index

`docs/index.md` is the source-of-truth map for agent-facing repository knowledge.
Use it to find the current architecture, testing workflow, debugging surfaces, and
checked-in execution plans without relying on stale notes.

## Start Here

- [docs/architecture.md](architecture.md): current subsystem map for the CLI, server, worker, sandbox, output, and eval surfaces.
- [docs/testing.md](testing.md): public validation surface, snapshot workflow, and eval harness entrypoints.
- [docs/debugging.md](debugging.md): debug logs, `--debug-repl`, and wire tracing.
- [docs/sandbox.md](sandbox.md): sandbox modes, writable roots, and client-driven sandbox updates.
- [docs/worker_sideband_protocol.md](worker_sideband_protocol.md): server/worker IPC contract.
- [docs/plans/README.md](plans/README.md): when to write a checked-in execution plan and where it lives.

## Normative Docs

- [docs/tool-descriptions/repl_tool.md](tool-descriptions/repl_tool.md): backend-neutral `repl` description.
- [docs/tool-descriptions/repl_tool_r.md](tool-descriptions/repl_tool_r.md): R-specific `repl` behavior.
- [docs/tool-descriptions/repl_tool_python.md](tool-descriptions/repl_tool_python.md): Python-specific `repl` behavior.
- [docs/tool-descriptions/repl_reset_tool.md](tool-descriptions/repl_reset_tool.md): `repl_reset` behavior.
- [README.md](../README.md): user-facing overview and installation guide. Treat it as product documentation, not the engineering source of truth.

## Exploratory Docs

- `docs/notes/`: ideas, sketches, and experimental eval design that may lead to later work.
- `docs/futurework/`: candidate follow-on designs that are not current repository contract.

## External or Vendored Material

- `eval/inspect_swe/`: third-party harness code that may exist in a local working tree for experimentation. Do not treat it as `mcp-repl` source of truth.

## Maintenance Rules

- Add new normative docs here in the same PR that introduces them.
- Keep [AGENTS.md](../AGENTS.md) short and use it as a pointer back to this index.
- Prefer moving completed execution plans into [docs/plans/completed/](plans/completed/) instead of leaving one-off plan files at the top of `docs/`.
