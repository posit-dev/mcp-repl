# Agent Map

Keep this file short. It is a table of contents, not the full manual.

## Immediate Rules

- If you modified code, run all required checks before replying:
  - `cargo check`
  - `cargo build`
  - `python3 tests/run_integration_tests.py --binary target/debug/mcp-repl`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test --quiet`
  - `cargo +nightly fmt`
- For docs-only changes, run the narrow docs validation that covers the edited
  files, usually `cargo test --test docs_contracts`.
- When changing Codex backend selection or CI real-client wiring, also run:
  - `MCP_REPL_CODEX_BACKEND=mock cargo test -j 1 --test codex_integration codex_exec_auto_backend_smoke -- --test-threads=1`
- Treat all clippy warnings as failures. Do not leave warning cleanup for later.
- Never pass `--vanilla` to `R` or `Rscript` unless the user explicitly asks for it.

## Start Here

- `docs/index.md`: source-of-truth map for repository docs.
- `docs/architecture.md`: subsystem map for the CLI, server, worker, sandbox, output, and validation surfaces.
- `docs/testing.md`: public verification surface and snapshot workflow.
- `docs/debugging.md`: debug logs, `--debug-repl`, and stdio tracing.
- `docs/sandbox.md`: sandbox modes and writable-root policy.
- `docs/output_timeline.md`: visible output ordering across sideband and raw streams.
- `docs/worker_sideband_protocol.md`: current server/worker IPC contract.
- `docs/plans/AGENTS.md`: when to create checked-in execution plans.

## Glossary

- Agent: The model-facing actor using an MCP client to call `repl` or `repl_reset`.
- MCP client: Codex, Claude, or another app that starts `mcp-repl` over MCP stdio and sends tool calls.
- Server: The main `mcp-repl` Rust process in MCP server mode. It owns the MCP surface, worker lifecycle, sandbox application, timeout policy, stdout/stderr capture, sideband interpretation, and response finalization.
- Worker: The child process spawned by the server to run the selected R or Python REPL. It runs inside the effective sandbox and owns the worker-side endpoint of sideband IPC.
- Worker child process: Any direct or indirect process spawned by user code or the backend under the worker. It may inherit stdout/stderr, but it must not own sideband IPC.
- Backend / interpreter: `backend` is the worker-side implementation that presents a selected REPL runtime to the server and MCP client. `interpreter` is the user-facing selector for that presented runtime, currently `r` or `python`; it does not describe the implementation language of the worker binary.
- Runtime: The live R or Python execution environment inside the worker. This is where client-submitted code via `repl` is evaluated.
- REPL session: The stateful runtime in the active worker. One session per worker process instance.
- Tool call: One MCP client invocation of `repl` or `repl_reset`.
- Request: The unit of input accepted by the server for the worker to execute. A request may outlive the initial tool call when it times out and later polls drain output.
- Reply: The MCP tool result returned to the client. Reply finalization is server-owned and may combine worker-originated content with server-only status notices.
- Poll: An empty `repl` input used to drain pending output, wait again on a previously timed-out request, return idle status, or advance pager mode.
- Host: The user's machine and OS environment outside the worker sandbox. Avoid `host-owned` unless the owner is explicitly distinguished from the MCP client, server, worker, and OS/user.
- Sandbox policy: The effective OS-level permissions applied to the worker: `read-only`, `workspace-write`, `danger-full-access`, or `external-sandbox`.
- Sandbox metadata: Codex per-tool-call `_meta["codex/sandbox-state-meta"]` used by `--sandbox inherit` to choose the effective worker sandbox for that call.
- Writable root: An absolute path that a `workspace-write` worker may write, subject to forced read-only subpaths like `.git`, `.codex`, and `.agents`.
- Session temp directory: The server-allocated per-session temp path exposed to the worker as `TMPDIR` and `MCP_REPL_R_SESSION_TMPDIR`.
- Sideband IPC: The JSON-lines server/worker pipe for structural facts such as `readline_start`, `readline_input`, `readline_discard`, `output_text`, `plot_image`, and `session_end`.
- Raw output capture: The stdout/stderr pipes or PTY stream captured by the server for unowned visible text. Sideband carries worker-owned text and structural facts.
- Output timeline: The server-side reconstruction of visible output order from captured stdout/stderr plus sideband facts.
- Server-owned: State, files, or notices created and retained by the main server process, not by the runtime or the worker. Use this for output bundles, response finalization, debug logs, and server temp roots.
- Worker-originated text: Text that came from the worker REPL or worker child processes and can be written to `transcript.txt`.
- Server-originated text: Status text synthesized by the server, such as timeout, busy, restart, sandbox, or bundle notices. Also called server-only text when contrasting with worker-originated transcript text.
- Output bundle: A server-owned directory for oversized (potentially mixed text/image) output in files mode, with a bounded inline preview plus inspectable files.
- `transcript.txt`: Bundle file containing worker-originated REPL text only, including echoed input, prompts, stdout, and rendered stderr text.
- `events.log`: Bundle index for mixed text/image history. `T` rows point into `transcript.txt`, `I` rows point to image history, and `S` rows are server-originated omission notices.
- Files mode / pager mode: `--oversized-output files` spills large replies into output bundles; `--oversized-output pager` keeps oversized text in an interactive pager that consumes tool-call input locally instead of forwarding it to the worker until the pager exits or reaches the end.
- Debug REPL: `--debug-repl`, a local interactive driver for the worker that bypasses MCP client/server traffic.
- Wire trace: The external stdio proxy log of exact bytes between an MCP client and the `mcp-repl` server.

## Snapshot Workflow

- Preferred loop:
  - `cargo insta test`
  - `cargo insta pending-snapshots`
  - `cargo insta review` or `cargo insta accept` / `cargo insta reject`
- CI-style validation: `cargo insta test --check`
- Do not add `--unreferenced=reject` to the general snapshot check; this
  repository keeps valid platform-specific snapshots that are unreferenced on
  other platforms.
- For broad intentional snapshot migrations: `cargo insta test --force-update-snapshots --accept`
- Do not delete `tests/snapshots/*.snap.new` manually. Use `cargo insta reject`.


## Planning Rule

- For multi-phase refactors, redesigns, or other work that spans discovery, iteration, and implementation, keep a living plan under `docs/plans/active/` until the initiative is complete.
- Use the plan to capture design decisions, rejected options, phase boundaries, unresolved questions, and the next safe slice of work so a later agent does not need to rediscover them.
- Plans and future-work notes should include a motivating task, use case, or MRE-style scenario before design details. Capture what the agent, MCP client, server, worker, or user should be able to do, then list constraints and options.
- If you pause or hand off work mid-task, update the plan before stopping.
- Do not create plan files for routine, obvious, or low-risk changes. Keep the plans area useful, not noisy.
- Move completed plans to `docs/plans/completed/`.
- Treat `docs/notes/` and `docs/futurework/` as exploratory, not normative.

## External References

- Consult `~/github/wch/r-source` for R behavior details.
- Consult `~/github/python/cpython` for Python behavior details.
