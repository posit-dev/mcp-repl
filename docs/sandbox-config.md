# Sandbox Configuration Semantics

This document describes how `mcp-repl` builds effective sandbox behavior from CLI flags,
`--config` overrides, client sandbox updates, and platform-specific enforcement.

## Scope

`mcp-repl` tracks sandbox state in three parts:

- `sandbox_policy`: filesystem + base network mode (`read-only`, `workspace-write`, `danger-full-access`, or `external-sandbox` via client update)
- `managed_network_policy`: managed network flags and domain lists (`allowed_domains`, `denied_domains`, `allow_local_binding`, `enabled`)
- feature flags such as `use_linux_sandbox_bwrap`

## Effective State Resolution

Operations are applied in argument order.

- `--sandbox ...` sets the base mode at that point in the sequence
- `--add-writable-root` mutates the current `workspace-write` roots
- `--add-allowed-domain` appends to managed `allowed_domains`
- `--config key=value` applies one structured mutation
- Later operations win when they set the same field

Important reset rule:

- `--sandbox` resets the policy to the chosen base mode and default values for that mode.

Failure rules are explicit (fail fast):

- `--sandbox inherit` fails if no client sandbox update was provided
- invalid key/value parsing fails (for example malformed JSON in `--config`)

Workspace-write-only operations are accepted in all modes, but apply only when the effective
policy is `workspace-write`:

- `--add-writable-root`
- `sandbox_workspace_write.network_access`
- `sandbox_workspace_write.writable_roots`
- `sandbox_workspace_write.exclude_tmpdir_env_var`
- `sandbox_workspace_write.exclude_slash_tmp`

## Presets and Defaults

Server default (no sandbox flags):

- `sandbox_policy = workspace-write`
- `sandbox_workspace_write.network_access = false`
- writable roots start empty and are expanded by runtime defaults (cwd/temp/session temp)

Install defaults:

- Codex install (`mcp-repl install --client codex`): injects `--sandbox inherit` unless sandbox args were explicitly supplied via `--arg`
- Claude install (`mcp-repl install --client claude`): injects `--sandbox workspace-write` unless sandbox args were explicitly supplied via `--arg`

R install-time writable root injection:

- For `r_repl`, installer probes `R` for the R cache root:
  `dirname(tools::R_user_dir("mcp_repl_install_probe", which = "cache"))`
- If detected and absolute, installer appends `--add-writable-root <path>`
- Injection is skipped when explicit sandbox config is supplied via install `--arg`

## Allowed Domains Semantics

`allowed_domains` and `denied_domains` are policy inputs for managed networking.

How to set them:

- CLI append: `--add-allowed-domain <pattern>`
- Structured replace: `--config permissions.network.allowed_domains=["..."]`
- Structured deny list: `--config permissions.network.denied_domains=["..."]`

Key behavior:

- Domain lists do not enable network by themselves.
  You still need a sandbox mode with network enabled (`workspace-write` + `sandbox_workspace_write.network_access=true`, or full-access modes).
- Domain entries are passed through as strings; `mcp-repl` does not parse wildcard/domain syntax.
- Domain restrictions are meaningful only when managed proxy routing is active and the proxy honors them.

## Platform Matrix

### macOS

Enforcement:

- Uses `sandbox-exec` policy generation.
- `workspace-write` includes:
  - configured writable roots
  - cwd
  - temp roots (`/tmp`, `TMPDIR` if absolute)
  - session temp root
- `.git`, `.codex`, and `.agents` subpaths are forced read-only when present inside writable roots.

Network behavior:

- If network is disabled in sandbox policy: no network.
- If network is enabled and neither proxy-managed mode nor domain restrictions are active, outbound/inbound network is allowed.
- If network is enabled and loopback proxy env vars are present, outbound is restricted to those loopback proxy endpoints.
- If managed mode/domain lists are set but no usable loopback proxy endpoint exists, policy fails closed (effectively no network).
- `ALLOW_LOCAL_BINDING=1` permits localhost bind/inbound traffic in proxy-managed mode.

### Linux

Enforcement:

- Uses the Linux sandbox helper (Landlock + seccomp).
- `workspace-write` always includes session temp dir.
- `read-only` is converted to a minimal writable policy for session temp dir.
- Optional outer `bwrap` stage is controlled by `MCP_CONSOLE_USE_LINUX_BWRAP=1`.

Network behavior:

- If network is disabled in sandbox policy: seccomp blocks network syscalls.
- If network is enabled and managed mode/domain lists are active:
  - Linux runs proxy-routed mode
  - loopback proxy env vars must be present and parseable
  - startup fails if proxy routing cannot be prepared
- In `bwrap` mode, proxy traffic is bridged into the namespace.

### Windows (experimental)

Enforcement:

- `read-only` and `workspace-write` use the Windows sandbox runner.
- `danger-full-access` and `external-sandbox` bypass built-in sandbox enforcement.
- Python backend is currently unavailable on Windows in this project.

Managed network and domains:

- Domain lists are not actively enforced by the Windows runner.
- Restricted network mode uses environment-level offline/proxy poisoning (`HTTP_PROXY=127.0.0.1:9`, etc.).
- Treat allowed/denied domain lists as non-authoritative on Windows today.

## Intersections and Gotchas

- `--add-allowed-domain` with default `workspace-write` still yields no network unless you also enable network access.
- `--sandbox inherit` + `--add-writable-root` is portable across inherited policies; if inherited mode is not `workspace-write`, the writable-root addition is a no-op.
- `danger-full-access` ignores sandbox enforcement, so domain controls are not enforced locally by `mcp-repl`.
- Managed-network behavior is proxy-driven; if there is no managed proxy path, domain controls may not be effective.

## Configuration Examples

Minimal network-restricted default:

```toml
[mcp_servers.r_repl]
command = "/Users/alice/.cargo/bin/mcp-repl"
args = [
  "--sandbox", "workspace-write",
  "--interpreter", "r",
]
```

Enable managed-domain network for Python:

```toml
[mcp_servers.py_repl]
command = "/Users/alice/.cargo/bin/mcp-repl"
args = [
  "--sandbox", "workspace-write",
  "--config", "sandbox_workspace_write.network_access=true",
  "--add-allowed-domain", "pypi.org",
  "--add-allowed-domain", "files.pythonhosted.org",
  "--interpreter", "python",
]
```

Install with the same domain settings:

```sh
mcp-repl install --client codex --interpreter python \
  --arg=--sandbox --arg=workspace-write \
  --arg=--config --arg='sandbox_workspace_write.network_access=true' \
  --arg=--add-allowed-domain --arg=pypi.org \
  --arg=--add-allowed-domain --arg=files.pythonhosted.org
```
