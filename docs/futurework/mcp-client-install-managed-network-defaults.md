# MCP Client Install Managed Network Defaults

## Motivation

After `mcp-repl install --client codex` or `mcp-repl install --client claude`,
common package and documentation tasks should work without requiring users to
hand-edit MCP client config first. The default install should give the worker a
narrow, managed network path to trusted package and documentation sources while
unrelated worker network access remains blocked.

Minimal task:

1. A user runs `mcp-repl install --client codex` or `mcp-repl install --client
   claude`.
2. The installer updates the selected MCP client config with the normal R and
   Python MCP servers and a curated managed-network allowlist.
3. The agent asks the R runtime to install or inspect a CRAN package, or asks
   the Python runtime to install or inspect a PyPI package.
4. The worker can reach the configured package and documentation hosts through
   managed networking.
5. The worker still cannot reach unrelated hosts.

This is a default-on usability improvement for the happy path, not a change to
the sandbox trust model.

## Smoke Scenarios

R package metadata:

```r
ap <- available.packages(repos = "https://cloud.r-project.org")
stopifnot("curl" %in% rownames(ap))
```

Python package metadata:

```python
import urllib.request

with urllib.request.urlopen("https://pypi.org/simple/") as response:
    assert response.status == 200
```

These are network smoke checks, not exhaustive package-manager tests. The same
run should also prove that an unrelated host fails closed.

## Candidate Sources

The exact list should be reviewed during implementation. A conservative starting
set is:

- CRAN/package metadata and R documentation: `cloud.r-project.org`,
  `cran.r-project.org`, `search.r-project.org`
- PyPI/package metadata and files: `pypi.org`, `files.pythonhosted.org`
- Python documentation: `docs.python.org`

Avoid broad wildcard defaults such as `**.python.org` or `**.r-project.org`
unless exact hosts prove insufficient for the target task. The installer should
prefer explicit, explainable entries over a large convenience allowlist.

## Current Shape

- MCP client install is written by `src/install.rs`.
- `mcp-repl install --client codex` currently writes MCP server entries with
  `--sandbox inherit --oversized-output files`.
- `--sandbox inherit` means the server resolves the effective worker sandbox
  from Codex per-tool-call sandbox metadata before applying later CLI/config
  operations.
- `mcp-repl install --client claude` currently writes Claude config with an
  explicit sandbox mode and `--oversized-output files`; Claude sandbox-inherit
  support is separate future work.
- Managed network domain configuration currently uses host patterns, not URLs.
  Exact URL entries such as `https://pypi.org/simple/` are rejected by the
  parser because path/query enforcement is not supported for ordinary HTTPS.
- On macOS, the managed proxy is available only when the effective sandbox is a
  built-in workspace-write sandbox with network access enabled and managed
  domain restrictions configured.

## Design Constraints

- Preserve each MCP client's sandbox contract. Do not add install defaults in a
  way that accidentally turns Codex installs into a fixed `workspace-write`
  sandbox, or that diverges from Claude's configured sandbox shape.
- Avoid forcing network configuration onto read-only tool calls. A naive
  `--config sandbox_workspace_write.network_access=true` appended after
  `--sandbox inherit` can fail when Codex sends read-only sandbox metadata,
  because that setting only applies to workspace-write policies.
- Decide whether defaults belong in the MCP client's own sandbox configuration,
  in `mcp-repl` server args, or in a new install-time helper that only affects
  workspace-write calls. The MCP client and `mcp-repl` should agree on the
  effective policy seen by the agent.
- Make the install idempotent. Re-running install for Codex or Claude should
  not duplicate allowlist entries or discard user-added entries.
- Provide a clear opt-out or override path before enabling defaults. Users who
  need fully offline operation, internal package mirrors, or stricter allowlists
  should not have to manually undo generated config.
- Treat package sources as code ingress. These defaults constrain egress; they
  do not make downloaded packages or documentation content trusted.

## Acceptance Shape

- Add failing install tests for Codex and Claude that assert the generated
  client config contains default managed-network entries in the chosen config
  location.
- For Codex, assert that generated MCP server entries still preserve
  `--sandbox inherit`.
- For Codex, assert that read-only inherited sandbox metadata does not fail
  solely because install added managed-network defaults for workspace-write
  calls.
- For Claude, assert that install defaults respect Claude's current explicit
  sandbox config shape and can later compose with Claude sandbox-inherit support.
- Add an idempotence test that runs install twice and asserts that allowlist
  entries are not duplicated.
- Add an override test that proves explicit user-provided network configuration
  is preserved or disables the default list according to the chosen contract.
- Add a small runtime scenario, preferably macOS-only while managed networking
  is macOS-only, that demonstrates an allowed package or documentation host
  succeeds while an unrelated host fails closed.
