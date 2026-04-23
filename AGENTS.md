# Agent Map

Keep this file short. It is a table of contents, not the full manual.

## Immediate Rules

- If you modified code, run all required checks before replying:
  - `cargo check`
  - `cargo build`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test`
  - `cargo +nightly fmt`
- Treat all clippy warnings as failures. Do not leave warning cleanup for later.
- Never pass `--vanilla` to `R` or `Rscript` unless the user explicitly asks for it.

## Start Here

- `docs/index.md`: source-of-truth map for repository docs.
- `docs/architecture.md`: subsystem map for the binary, worker, sandbox, and eval surfaces.
- `docs/testing.md`: public verification surface and snapshot workflow.
- `docs/debugging.md`: debug logs, `--debug-repl`, and stdio tracing.
- `docs/sandbox.md`: sandbox modes and writable-root policy.
- `docs/plans/AGENTS.md`: when to create checked-in execution plans.

## Snapshot Workflow

- Preferred loop:
  - `cargo insta test`
  - `cargo insta pending-snapshots`
  - `cargo insta review` or `cargo insta accept` / `cargo insta reject`
- CI-style validation: `cargo insta test --check --unreferenced=reject`
- For broad intentional snapshot migrations: `cargo insta test --force-update-snapshots --accept`
- Do not delete `tests/snapshots/*.snap.new` manually. Use `cargo insta reject`.


## Review Loop

- Run `codex review` against the actual PR base, not just `main`.
- Capture stdout and stderr separately: findings go to stdout; progress logs go to stderr.
- Run review as one blocking command:
  - `codex review --base <base> > /tmp/<name>.stdout 2> /tmp/<name>.stderr`
- Give that command a timeout of at least 30 minutes and let it run to completion.
- Only poll if the execution tool times out before the process exits. If polling is necessary, poll no more than once every 5-10 minutes.
- Treat review findings as implementation, test-isolation, or docs-clarity work.
- If fixing a finding would change the intended contract, rather than implement it, stop and ask the user.
- If fixing a finding would broaden the PR beyond its intended scope, stop and ask the user unless it is clearly critical.
- Fix real findings one at a time. Add or adjust the regression test first when practical, keep the branch scope contained, then rerun the required Rust checks. In the commit message for each fix, include the full verbatim finding, as well as the response to how the finding was addressed.
- After each set of review-fix commits, rerun `codex review` against the same base until stdout reports no actionable findings.
- If the diff meaning or composition changed during the review loop, update the PR body so it still matches the branch.
- For large PRs, generate the `Diff composition` section with:
  - `python3 scripts/diff_composition.py --base <base> --head HEAD --format markdown`
- Paste that summary into the PR body so reviewers can see how much of the PR is behavior change versus coverage or documentation.


## Planning Rule

- For multi-phase refactors, redesigns, or other work that spans discovery, iteration, and implementation, keep a living plan under `docs/plans/active/` until the initiative is complete.
- Use the plan to capture design decisions, rejected options, phase boundaries, unresolved questions, and the next safe slice of work so a later agent does not need to rediscover them.
- If you pause or hand off work mid-task, update the plan before stopping.
- Do not create plan files for routine, obvious, or low-risk changes. Keep the plans area useful, not noisy.
- Move completed plans to `docs/plans/completed/`.
- Treat `docs/notes/` and `docs/futurework/` as exploratory, not normative.

## External References

- Consult `~/github/wch/r-source` for R behavior details.
- Consult `~/github/python/cpython` for Python behavior details.
