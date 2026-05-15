# Snapshot (golden) tests

This crate uses `insta` snapshots for MCP tool-call transcripts.
Default tests should exercise the built `mcp-repl` binary through the public MCP
tool surface. Keep direct internal tests rare and use them only to document
intent that cannot be reached from a public tool call.

- Preferred workflow:
  - `cargo insta test`
  - `cargo insta pending-snapshots`
  - `cargo insta review` (or `cargo insta accept` / `cargo insta reject` for non-interactive runs)
- CI-style check: `cargo insta test --check --unreferenced=reject`
- For format/metadata migrations: `cargo insta test --force-update-snapshots --accept`
- Bulk rewrite fallback: `INSTA_UPDATE=always cargo test`
- Review accepted changes with `git diff tests/snapshots`.
- Transcript snapshots live alongside the JSON snapshots with `@transcript` suffixes.
- Do not manually delete `tests/snapshots/*.snap.new`; use `cargo insta reject`.
- Set `MCP_REPL_RUN_CLIENT_INTEGRATIONS=1` only when explicitly running real
  Codex or Claude client integration tests.

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
