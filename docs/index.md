# Docs Index

`docs/index.md` is the source-of-truth map for agent-facing repository knowledge.
Use it to find the current architecture, testing workflow, debugging surfaces, and
checked-in execution plans without relying on stale notes.

## Start Here

- `docs/architecture.md`: current subsystem map for the CLI, server, worker, sandbox, and output surfaces.
- `docs/output_timeline.md`: server-side model for merging text pipes and sideband events into visible reply order.
- `docs/testing.md`: public validation surface and snapshot workflow.
- `docs/debugging.md`: debug logs, `--debug-repl`, and wire tracing.
- `docs/sandbox.md`: sandbox modes, writable roots, and Codex per-tool-call sandbox metadata.
- `docs/worker_sideband_protocol.md`: server/worker IPC contract.
- `docs/plans/AGENTS.md`: when to write a checked-in execution plan and where it lives.
- `docs/plans/completed/codex-sandbox-state-meta-migration.md`: completed plan for migrating Codex `--sandbox inherit` from async updates to per-tool-call sandbox metadata.

## Normative Docs

- `docs/tool-descriptions/repl_tool.md`: explains how `repl` tool descriptions are selected by backend and oversized-output mode.
- `docs/tool-descriptions/repl_tool_r.md`: R `repl` behavior for the files-mode oversized-output path.
- `docs/tool-descriptions/repl_tool_r_pager.md`: R `repl` behavior for pager mode.
- `docs/tool-descriptions/repl_tool_python.md`: Python `repl` behavior for the files-mode oversized-output path.
- `docs/tool-descriptions/repl_tool_python_pager.md`: Python `repl` behavior for pager mode.
- `docs/tool-descriptions/repl_reset_tool.md`: `repl_reset` behavior.
- `README.md`: user-facing overview and installation guide. Treat it as product documentation, not the engineering source of truth.

## Exploratory Docs

- `docs/notes/`: ideas and sketches that may lead to later work.
- `docs/futurework/`: candidate follow-on designs that are not current repository contract.
- `docs/futurework/advisory-worker-write-observations.md`: deferred note on emitting best-effort IPC metadata for worker-owned stdout/stderr writes without replacing pipe capture.
- `docs/futurework/composable-tool-descriptions.md`: deferred design note on replacing the current multi-file `repl` description matrix with one composable template plus runtime interpolation.
- `docs/futurework/offline-manual-surfaces.md`: deferred design note on exposing R manuals and future Python manuals through better offline model-facing surfaces than the current inline `RShowDoc()` flow.
- `docs/futurework/per-turn-history-bundles.md`: design brief for always-materialized per-turn REPL history bundles.
- `docs/futurework/r-embedding-minimal-callbacks.md`: deferred note on reducing custom embedded-R callbacks while keeping readline integration.
- `docs/futurework/r-graphics-device-for-incremental-plot-emission.md`: deferred design note on replacing hook/replay plot capture with a device-level path that can emit plots before grouped expressions finish.
- `docs/futurework/worker-session-tempdir-rotation.md`: deferred design note on rotating worker tempdir paths per launch so stale temp trees do not block respawn.
- `docs/futurework/stronger-worker-child-containment.md`: deferred design note on tighter worker descendant containment, especially on Windows.
- `docs/futurework/unified-output-timeline-pipeline.md`: deferred design note for converging pager and files mode onto one shared resolved timeline pipeline.
- `docs/futurework/stdin-transport-single-owner.md`: deferred design for making worker stdin ownership explicit instead of relying on a Windows-only gate.
- `docs/futurework/repl-interaction-rough-edges.md`: candidate UX polish items observed during live REPL use.

## Maintenance Rules

- Add new normative docs here in the same PR that introduces them.
- Keep `AGENTS.md` short and use it as a pointer back to this index.
- Prefer moving completed execution plans into `docs/plans/completed/` instead of leaving one-off plan files at the top of `docs/`.
