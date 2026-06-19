# Sandbox

`mcp-repl` applies an OS sandbox to worker processes unless the sandbox policy is
`danger-full-access` (or `external-sandbox`).

## Default policy

When no CLI sandbox mode is provided, the default is:

- `workspace-write`
- `network_access: false`

When `--sandbox inherit` is used for MCP server operation, the MCP client must
attach per-tool-call sandbox metadata in `_meta["codex/sandbox-state-meta"]`.
That metadata is the source of truth for the tool call that is about to run. If
it is missing or malformed, `mcp-repl` fails closed with `--sandbox inherit
requested but no client sandbox state was provided`.

`--debug-repl` is the one local-only exception. Because there is no MCP client
metadata channel in that mode, `mcp-repl --debug-repl --sandbox inherit`
bootstraps one local inherited snapshot from the current default sandbox state
before the first worker spawn.

For `repl`, inherited sandbox metadata controls the worker session that handles
the call. When a non-empty tool call would use the worker and the effective
inherited sandbox changed, `mcp-repl` restarts the worker before serving that
call and includes a restart notice that names the new sandbox policy.

More specifically:

- Empty-input polls ignore per-call sandbox metadata while they are only
  draining existing pending or settled output, or returning an idle prompt from
  an already-running worker.
- If an empty-input poll needs to spawn or respawn a worker to finish answering
  the call, `mcp-repl` applies the current tool call's metadata before that
  spawn. If a poll can first answer by draining a session-ended request, it
  returns that local drain without respawning; the next spawn-needed call must
  provide valid current metadata.
- While the pager is active, pure pager navigation is local UI state, not a
  worker interaction. Pager-local commands such as `:q` or empty-string page
  advance ignore sandbox metadata until a later tool call actually interacts
  with the worker again. Bare `Ctrl-D` is not pager navigation; it remains an
  explicit restart even when the pager is active.
- Bare `Ctrl-C` is the one non-empty `repl` follow-up that stays local for
  sandbox metadata and does not force a sandbox-driven restart. If a worker
  process already exists, the interrupt is still forwarded to that worker.
- Every other non-empty `repl` call must have valid current
  `_meta["codex/sandbox-state-meta"]`.
- A non-empty retry after the memory guardrail aborts a worker is an ordinary
  non-empty call. It must have valid current metadata before `mcp-repl` resets
  or retries under `--sandbox inherit`.
- Non-empty `repl` calls resolve stale timeout markers before deciding whether
  they are still looking at a live worker request.
- If current metadata changes the effective inherited sandbox, `mcp-repl`
  restarts the worker at that call before handling the input.
- Control-prefixed tails such as `Ctrl-C<code>` and `Ctrl-D<code>` run in the
  restarted session when the sandbox changed; the control prefix itself is not
  replayed into the fresh worker.
- Explicit restarts discard preserved detached output from aborted prior
  requests instead of carrying it into later unrelated replies.
- Sandbox metadata is enforced again at the next tool call that actually
  interacts with the worker after pager navigation ends.
- Missing or malformed metadata still fails closed on calls that need it.

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
  `mcp-repl --config key=value` with documented sandbox/config keys
- MCP sandbox metadata capability:
  `codex/sandbox-state-meta` (advertised only when the effective CLI sandbox mode still resolves to `inherit` after later overrides)

Operations are applied strictly in CLI argument order. Later operations win.
`--sandbox ...` resets the base policy at the point where it appears.

## macOS behavior

Sandboxing is enforced via `sandbox-exec`.

For `workspace-write`, writable roots include:

- configured `writable_roots` (absolute paths only),
- current working directory,
- R cache roots configured in MCP client policy,
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
- when allowed or denied domains are configured, the server starts a managed
  HTTP/SOCKS proxy on loopback, injects proxy env vars into the worker, and
  permits Seatbelt egress only to that proxy,
- `MCP_REPL_MANAGED_NETWORK=1` enforces proxy-only mode for an externally
  configured loopback proxy,
- domain rules support exact hosts, `*.example.com` subdomains, and
  `**.example.com` for the apex plus subdomains,
- exact URLs such as `https://pypi.org/simple/` are rejected; HTTPS proxying
  can enforce the `CONNECT` host but not URL paths,
- `ALLOW_LOCAL_BINDING=1` additionally allows localhost bind/inbound operations.

Example PyPI allowlist:

```sh
mcp-repl --sandbox workspace-write \
  --config sandbox_workspace_write.network_access=true \
  --add-allowed-domain pypi.org \
  --add-allowed-domain files.pythonhosted.org
```

Example CRAN allowlist:

```sh
mcp-repl --sandbox workspace-write \
  --config sandbox_workspace_write.network_access=true \
  --add-allowed-domain cran.r-project.org \
  --add-allowed-domain cloud.r-project.org \
  --add-allowed-domain '**.r-project.org'
```

## Linux behavior

Sandboxing is enforced by a Linux sandbox helper that applies seccomp + Landlock.

- `workspace-write` always includes the per-session temp directory in writable roots.
- `read-only` is translated to a minimal writable setup for the session temp directory only.
- default Linux worker setup disables network unless explicitly enabled.
- managed domain allowlists are not enforced on Linux yet; configuring allowed
  or denied domains with enabled network access currently fails closed.
- `mcp-repl` always uses its own internal Linux sandbox launcher; helper
  executable paths provided by an MCP client are ignored.
- Codex sandbox metadata does not control `mcp-repl`'s optional internal
  `bwrap` stage. That remains a local best-effort setting.

Optional `bwrap` stage:

- `MCP_REPL_USE_LINUX_BWRAP=1` enables a bubblewrap outer sandbox.
- `MCP_REPL_LINUX_BWRAP_NO_PROC=1` skips `/proc` mounting.
- if `bwrap` is requested but worker startup dies before backend info arrives,
  `mcp-repl` retries once without `bwrap` and continues.

## Windows behavior (experimental)

- R backend is supported with the same policy surface (`read-only`, `workspace-write`, `danger-full-access`).
- Python support is not part of the stable Windows surface yet. The embedded
  backend no longer requires a Unix PTY, but Windows support still depends on
  the selected CPython installation exposing a loadable runtime library.
- network-restricted and managed-domain Windows sandboxes require one-time
  elevated setup:

  ```powershell
  mcp-repl windows-sandbox setup --http-proxy-port 39080 --socks-proxy-port 39081
  ```

  Setup creates or refreshes the local non-admin `McpReplOffline`
  account, stores its password with current-user DPAPI protection under
  `%LOCALAPPDATA%\mcp-repl\windows-sandbox\`, and installs outbound firewall
  block rules scoped to that account SID.
- `workspace-write` with `network_access=true` and no managed domain rules keeps
  the current-user sandbox path and allows network access normally.
- `read-only`, default `workspace-write`, and managed-domain `workspace-write`
  launches run the Windows wrapper through the offline account. Firewall rules
  block non-loopback outbound traffic and loopback traffic except the configured
  managed proxy ports.
- when allowed or denied domains are configured on Windows, the server starts
  the same managed HTTP/SOCKS proxy used on macOS on the fixed setup ports and
  injects proxy env vars into the worker. Missing setup, stale setup, or occupied
  fixed proxy ports fail closed with an actionable error.
- `read-only` and `workspace-write` use a two-stage Windows sandbox model:
  - the parent prepares and reuses stable filesystem ACL state for the effective sandbox policy,
  - the internal Windows wrapper requires prepared launch state and applies launch-scoped ACLs for the worker run.
- Worker spawn refreshes prepared workspace ACL coverage before launch.
- The per-session temp directory stays launch-scoped and is not shared through the stable workspace SID; the same configured path may be reused across respawns, but it is reset before each fresh worker launch.
- `danger-full-access` and `external-sandbox` run without built-in sandbox enforcement.
- Some Windows environments may not support the restricted-token setup required by sandboxed modes.
