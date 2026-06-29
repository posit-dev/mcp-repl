# Managed Network Proxy

## Summary

- Add a managed network proxy for sandboxed workers.
- Keep existing full-network sandbox behavior unchanged when no managed domain rules are configured.
- Support Linux managed-domain enforcement on the bubblewrap sandbox path.
- Windows uses a setup-backed offline account plus account-scoped firewall rules
  and loopback WFP filters to route managed-domain traffic through the
  server-owned proxy.

## Status

- State: active
- Last updated: 2026-06-28
- Current phase: follow-up scoping after Linux enforcement

## Current Direction

- Use a server-owned HTTP/SOCKS proxy, proxy environment variables injected into the worker, and macOS Seatbelt egress limited to the proxy's loopback ports.
- On Linux, use bubblewrap `--unshare-net`, namespace-local bridge listeners on
  `127.0.0.1:39080` and `127.0.0.1:39081`, and Unix relay sockets under the
  session temp directory to connect those bridge listeners back to the
  server-owned proxy.
- Use host/domain matching only for this slice. Exact URL/path filtering is out of scope because normal HTTPS proxying exposes only the `CONNECT host:port` target unless a MITM certificate flow is added.
- Keep the code small and local to `mcp-repl`: use a simple proxy runtime tailored to package-install workflows.

## Long-Term Direction

- Linux routes worker traffic through a server-owned proxy from inside the
  bubblewrap network namespace without allowing direct worker egress.
- Windows uses the same policy surface after `mcp-repl windows-sandbox setup`
  installs the offline account, firewall rules, and loopback WFP filters.
- A future UI or approval flow can amend allow/deny rules, but this phase only supports static CLI/config rules.
- A future HTTP policy layer may support method restrictions such as "allow
  GET but deny POST", but that is separate from the current host/domain
  allowlist because ordinary HTTPS proxying does not expose the inner HTTP
  method without TLS interception or a protocol-specific path.

## Phase Status

- Phase 0: completed - chose the managed proxy shape and enforcement boundary.
- Phase 1: completed - implemented macOS managed proxy and public tests.
- Phase 2: completed - Linux bwrap-backed managed-domain enforcement.
- Phase 3: completed - Windows enforcement.

## Locked Decisions

- macOS enforcement uses Seatbelt loopback-only egress to the managed proxy ports.
- Windows enforcement uses a dedicated `McpReplOffline` local account,
  account-scoped firewall block rules, loopback WFP filters, and fixed managed
  proxy ports from setup.
- Domain policy is deny-first and allowlist-based.
- Supported patterns are exact hosts, `*.example.com`, and `**.example.com`.
- Exact URLs are rejected instead of being silently reduced to hosts.
- Proxy-aware tools are the transparency target; tools that ignore proxy env vars fail closed.
- Linux managed-domain enforcement requires bubblewrap. Legacy Landlock remains
  available only for non-managed-domain Linux sandbox modes.
- `mcp-repl` itself cannot request a permission escalation from an MCP client UI.
  User-approved network changes must happen through external client config,
  CLI args, project-local config, or a later non-MCP approval surface.

## Open Questions

- Whether a future HTTPS MITM mode is worth the certificate-management surface for URL/path-level filtering.
- Whether a GET-only web policy is useful enough to justify TLS visibility work,
  or whether package mirrors and host/port restrictions cover the practical
  use cases.
- Which managed-network follow-up slice should land first: explicit database TCP connect, local Shiny bind/inbound support, TLS SNI gating for `CONNECT`, SOCKS removal/gating, or split local connect/bind controls. See `docs/futurework/managed-network-follow-up.md`.

## Next Safe Slice

- Pick one follow-up from `docs/futurework/managed-network-follow-up.md`, such
  as TLS SNI gating for `CONNECT`, explicit database TCP allowlists, local
  Shiny/local-service bind support, or split local connect/bind controls.
- Keep the same public policy surface until a follow-up explicitly justifies a
  new rule shape.
- Preserve fail-closed behavior when domain rules are configured but the active
  sandbox mode cannot enforce them.

## Stop Conditions

- Stop and ask if a fix requires broadening this phase beyond macOS enforcement.
- Stop and ask if package-install support requires HTTPS path filtering instead of host/domain allowlisting.
- Stop and ask if managed proxy support would weaken existing `network_access=false` behavior.

## Decision Log

- 2026-04-30: Chose a server-owned proxy with environment-variable routing plus OS sandbox egress limits rather than transparent kernel redirection.
- 2026-04-30: Scoped matching to host/domain patterns after deciding exact HTTPS URL filtering would require a separate MITM design.
- 2026-04-30: Implemented the macOS slice with a small in-process HTTP/SOCKS proxy in `src/managed_network.rs`, CLI/config validation for host patterns, and worker launch wiring that injects proxy env vars before Seatbelt policy rendering.
- 2026-05-01: Documented managed-network follow-up scenarios and tradeoffs for package, database, Shiny, local-service, and hardening workflows.
- 2026-06-19: Implemented the Windows slice with explicit elevated setup, DPAPI-protected offline account credentials, fixed proxy ports, firewall rules scoped to the offline account SID, loopback WFP filters for direct local socket blocking, and offline-wrapper launch for workspace-write no-network or managed-domain sandbox policies.
- 2026-06-28: Implemented the Linux slice on the bubblewrap path. The worker
  runs in an isolated network namespace, bridge children listen on
  namespace-local fixed proxy ports, Unix relay sockets under session temp
  connect those bridge listeners to the server-owned proxy, and bwrap fallback
  is disabled for managed-domain launches.
