# Sandbox

`mcp-repl` applies an OS sandbox to worker processes unless the sandbox policy is
`danger-full-access` (or `external-sandbox`).

## Default policy

When no CLI sandbox mode is provided, the default is:

- `workspace-write`
- `network_access: false`

When `--sandbox inherit` is used, the client must send a sandbox update
(`codex/sandbox-state/update`) to override the worker sandbox policy for the session.
If no update is provided, `mcp-repl` falls back to its internal default policy
until an update arrives.

The worker also gets a per-session temp directory, exported as:

- `TMPDIR`
- `MCP_REPL_R_SESSION_TMPDIR`

## Configure sandbox policy

- Base mode: `mcp-repl --sandbox inherit|read-only|workspace-write|danger-full-access`
- Add writable roots (workspace-write only, repeatable):
  `mcp-repl --add-writable-root /absolute/path`
- Add allowed domains (repeatable):
  `mcp-repl --add-allowed-domain <pattern>`
- Advanced overrides:
  `mcp-repl --config key=value` with Codex-shaped keys
- MCP sandbox update method:
  `codex/sandbox-state/update` (capability `codex/sandbox-state`)

Operations are applied strictly in CLI argument order. Later operations win.
`--sandbox ...` resets the base policy at the point where it appears.

## macOS behavior

Sandboxing is enforced via `sandbox-exec`.

For `workspace-write`, writable roots include:

- configured `writable_roots` (absolute paths only),
- current working directory,
- R cache roots configured in client policy,
- temp roots (`/tmp`, `TMPDIR` when absolute), and
- the per-session temp directory.

If you also need R data/config roots, add them explicitly with repeatable
`--add-writable-root` entries.

Within writable roots, these subpaths are forced read-only when present:

- `.git`
- `.codex`
- `.agents`

Proxy-aware network behavior when `network_access: true`:

- proxy env vars are inspected (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and lowercase variants),
- loopback proxy endpoints are allowlisted for outbound traffic,
- proxy configured but no usable loopback endpoint => fail closed (no network),
- `MCP_REPL_MANAGED_NETWORK=1` enforces proxy-only mode,
- `ALLOW_LOCAL_BINDING=1` additionally allows localhost bind/inbound operations.

## Linux behavior

Sandboxing is enforced by a Linux sandbox helper that applies seccomp + Landlock.

- `workspace-write` always includes the per-session temp directory in writable roots.
- `read-only` is translated to a minimal writable setup for the session temp directory only.
- default Linux worker setup disables network unless explicitly enabled.
- `mcp-repl` always uses its own internal Linux sandbox launcher; client-provided
  helper executable paths are ignored.
- inherited `useLegacyLandlock` updates are translated onto `mcp-repl`'s
  internal `bwrap` on/off choice.

Optional `bwrap` stage:

- `MCP_REPL_USE_LINUX_BWRAP=1` enables a bubblewrap outer sandbox.
- `MCP_REPL_LINUX_BWRAP_NO_PROC=1` skips `/proc` mounting.
- if `bwrap` is requested but worker startup dies before backend info arrives,
  `mcp-repl` retries once without `bwrap` and continues.

## Windows behavior (experimental)

- R backend is supported with the same policy surface (`read-only`, `workspace-write`, `danger-full-access`).
- Python backend is currently unavailable on Windows (it requires a Unix PTY).
- `read-only` and `workspace-write` use a two-stage Windows sandbox model:
  - the parent prepares and reuses stable filesystem ACL state for the effective sandbox policy,
  - the internal Windows wrapper requires prepared launch state and applies launch-scoped ACLs for the worker run.
- Worker spawn refreshes prepared workspace ACL coverage before launch.
- The per-session temp directory stays launch-scoped and is not shared through the stable workspace SID; the same configured path may be reused across respawns, but it is reset before each fresh worker launch.
- `danger-full-access` and `external-sandbox` run without built-in sandbox enforcement.
- Some Windows environments may not support the restricted-token setup required by sandboxed modes.
