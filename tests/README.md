# Snapshot (golden) tests

This crate uses `insta` snapshots for MCP tool-call transcripts.

- Preferred workflow:
  - `cargo insta test`
  - `cargo insta pending-snapshots`
  - `cargo insta review` (or `cargo insta accept` / `cargo insta reject` for non-interactive runs)
- CI-style check: `cargo insta test --check`
- Do not add `--unreferenced=reject` to the general snapshot check; this
  repository keeps valid platform-specific snapshots that are unreferenced on
  other platforms.
- For format/metadata migrations: `cargo insta test --force-update-snapshots --accept`
- Review accepted changes with `git diff tests/snapshots`.
- Transcript snapshots live alongside the JSON snapshots with `@transcript` suffixes.
- Do not manually delete `tests/snapshots/*.snap.new`; use `cargo insta reject`.

New tests should prefer recording a sequence of `call_tool` invocations against a
single `McpSnapshot::session(...)`, and snapshotting the rendered transcript.

Example:

```rust
snapshot.session("default", mcp_script! {
    write_stdin("x <- 1");
    write_stdin("x <- x + 2");
    write_stdin("x", timeout = 0.2);
    write_stdin("\u{4}");
}).await?;
```
