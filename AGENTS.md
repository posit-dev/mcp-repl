# Agent Map

Keep this file short. It is a table of contents, not the full manual.

## Immediate Rules

- If you modified code, run all required checks before replying:
  - `cargo check`
  - `cargo build`
  - `cargo clippy`
  - `cargo test`
  - `cargo +nightly fmt`
- Never pass `--vanilla` to `R` or `Rscript` unless the user explicitly asks for it.

## Start Here

- [docs/index.md](docs/index.md): source-of-truth map for repository docs.
- [docs/architecture.md](docs/architecture.md): subsystem map for the binary, worker, sandbox, and eval surfaces.
- [docs/testing.md](docs/testing.md): public verification surface and snapshot workflow.
- [docs/debugging.md](docs/debugging.md): debug logs, `--debug-repl`, and stdio tracing.
- [docs/sandbox.md](docs/sandbox.md): sandbox modes and writable-root policy.
- [docs/plans/README.md](docs/plans/README.md): when to create checked-in execution plans.

## Snapshot Workflow

- Preferred loop:
  - `cargo insta test`
  - `cargo insta pending-snapshots`
  - `cargo insta review` or `cargo insta accept` / `cargo insta reject`
- CI-style validation: `cargo insta test --check --unreferenced=reject`
- For broad intentional snapshot migrations: `cargo insta test --force-update-snapshots --accept`
- Do not delete `tests/snapshots/*.snap.new` manually. Use `cargo insta reject`.

## Planning Rule

- Use a checked-in plan under `docs/plans/active/` for non-trivial multi-file work, protocol changes, cross-backend behavior changes, or work that spans more than one PR.
- Move completed plans to `docs/plans/completed/`.
- Treat `docs/notes/` and `docs/futurework/` as exploratory, not normative.

## External References

- Consult `~/github/wch/r-source` for R behavior details.
- Consult `~/github/python/cpython` for Python behavior details.
