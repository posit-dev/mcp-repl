# Test Helper Classification Consolidation

## Summary

Several integration tests classify backend startup failures, busy replies, and
sandbox availability by matching repeated lists of rendered text snippets. Those
helpers should be consolidated behind shared test predicates so individual tests
do not grow new local lists of equivalent string variants.

## Motivation

The immediate trigger was a Linux CI failure where a bubblewrap feature probe
reported the same mount class with a different stderr detail string. The fix for
that case belongs in the stable classifier: match the `bwrap` old-root to
new-root bind failure shape, not a list of exact bwrap detail messages or
temporary directory paths.

The same maintenance problem appears more broadly in test helpers. Many test
files carry local versions of:

- backend/runtime unavailable classification,
- busy response classification,
- sandbox or bubblewrap unavailable classification.

When those lists diverge, CI failures become harder to interpret because each
test file has its own notion of an ignorable environment failure.

## Observed Candidates

- `tests/common/mod.rs` already exposes `backend_unavailable()` and
  `is_busy_response()`.
- Local `backend_unavailable()` helpers appear in many files, including
  `tests/manage_session_behavior.rs`, `tests/pager.rs`, `tests/r_help.rs`,
  `tests/r_startup.rs`, `tests/write_stdin_behavior.rs`, and related pager/R
  tests.
- Local busy-response helpers appear in files such as `tests/r_startup.rs`,
  `tests/python_backend.rs`, and `tests/interrupt.rs`.
- Linux bubblewrap availability matching is split between `tests/sandbox.rs`
  and `tests/codex_integration.rs`.

## Intended Direction

- Move shared classification into `tests/common/mod.rs`.
- Prefer helpers that describe the stable class being detected, for example
  `backend_unavailable(text)`, `is_busy_response(text)`, and
  `linux_bwrap_unavailable(text)`.
- Keep test-specific exceptions local only when they express a unique contract
  for that test.
- Add focused unit tests for classification boundaries when a helper matches a
  broad class of external stderr or protocol text.
- Do not match temporary path names or runner-specific detail strings unless the
  test is explicitly about path rendering.

## Non-Goals

- Rewriting output snapshots.
- Hiding real product errors behind broad skip logic.
- Changing public reply text.
- Reclassifying failures that should stay hard failures.

## Suggested Slice

Start with one duplicated family, such as `backend_unavailable()`. Replace local
helpers with `tests/common/mod.rs` one file at a time, then run the affected
test file before moving to the next. Save Linux bwrap-specific consolidation for
a separate slice so sandbox-specific behavior remains easy to review.

## Verification

For a narrow docs-only update, run:

```sh
cargo test --test docs_contracts
```

For the actual test-helper refactor, run the changed test files directly and
finish with:

```sh
cargo test --quiet
```
