# Managed Network Proxy

## Summary

- Add a macOS-first managed network proxy for workers.
- Keep existing full-network sandbox behavior unchanged when no managed domain rules are configured.
- Keep Linux as an explicit follow-up phase with clear unsupported errors for managed domain enforcement.
- Windows uses a setup-backed offline account plus account-scoped firewall rules to route managed-domain traffic through the server-owned proxy.

## Status

- State: active
- Last updated: 2026-06-19
- Current phase: Linux planning after Windows enforcement

## Current Direction

- Use a server-owned HTTP/SOCKS proxy, proxy environment variables injected into the worker, and macOS Seatbelt egress limited to the proxy's loopback ports.
- Use host/domain matching only for this slice. Exact URL/path filtering is out of scope because normal HTTPS proxying exposes only the `CONNECT host:port` target unless a MITM certificate flow is added.
- Keep the code small and local to `mcp-repl`: use a simple proxy runtime tailored to package-install workflows.

## Long-Term Direction

- Linux should route worker traffic through a server-owned proxy from inside the Linux sandbox without allowing direct egress.
- Windows uses the same policy surface after `mcp-repl windows-sandbox setup` installs the offline account and firewall rules.
- A future UI or approval flow can amend allow/deny rules, but this phase only supports static CLI/config rules.
- A future HTTP policy layer may support method restrictions such as "allow
  GET but deny POST", but that is separate from the current host/domain
  allowlist because ordinary HTTPS proxying does not expose the inner HTTP
  method without TLS interception or a protocol-specific path.

## Phase Status

- Phase 0: completed - chose the managed proxy shape and enforcement boundary.
- Phase 1: completed - implemented macOS managed proxy and public tests.
- Phase 2: pending - Linux enforcement.
- Phase 3: completed - Windows enforcement.

## Locked Decisions

- macOS enforcement uses Seatbelt loopback-only egress to the managed proxy ports.
- Windows enforcement uses a dedicated `McpReplOffline` local account,
  account-scoped firewall block rules, and fixed managed proxy ports from setup.
- Domain policy is deny-first and allowlist-based.
- Supported patterns are exact hosts, `*.example.com`, and `**.example.com`.
- Exact URLs are rejected instead of being silently reduced to hosts.
- Proxy-aware tools are the transparency target; tools that ignore proxy env vars fail closed.
- `mcp-repl` itself cannot request a permission escalation from an MCP client UI.
  User-approved network changes must happen through external client config,
  CLI args, project-local config, or a later non-MCP approval surface.

## Open Questions

- Which Linux routing mechanism should become the long-term implementation: the existing internal sandbox helper, a socket bridge, or a separate network namespace path.
- Whether a future HTTPS MITM mode is worth the certificate-management surface for URL/path-level filtering.
- Whether a GET-only web policy is useful enough to justify TLS visibility work,
  or whether package mirrors and host/port restrictions cover the practical
  use cases.
- Which managed-network follow-up slice should land first: explicit database TCP connect, local Shiny bind/inbound support, TLS SNI gating for `CONNECT`, SOCKS removal/gating, or split local connect/bind controls. See `docs/futurework/managed-network-follow-up.md`.

## Next Safe Slice

- Design the Linux routing path for managed domain enforcement.
- Keep the same public policy surface: exact hosts, `*.example.com`, and `**.example.com`.
- Preserve fail-closed behavior when domain rules are configured but a platform cannot enforce them.

## Stop Conditions

- Stop and ask if a fix requires broadening this phase beyond macOS enforcement.
- Stop and ask if package-install support requires HTTPS path filtering instead of host/domain allowlisting.
- Stop and ask if managed proxy support would weaken existing `network_access=false` behavior.

## Decision Log

- 2026-04-30: Chose a server-owned proxy with environment-variable routing plus OS sandbox egress limits rather than transparent kernel redirection.
- 2026-04-30: Scoped matching to host/domain patterns after deciding exact HTTPS URL filtering would require a separate MITM design.
- 2026-04-30: Implemented the macOS slice with a small in-process HTTP/SOCKS proxy in `src/managed_network.rs`, CLI/config validation for host patterns, and worker launch wiring that injects proxy env vars before Seatbelt policy rendering.
- 2026-05-01: Documented managed-network follow-up scenarios and tradeoffs for package, database, Shiny, local-service, and hardening workflows.
- 2026-06-19: Implemented the Windows slice with explicit elevated setup, DPAPI-protected offline account credentials, fixed proxy ports, firewall rules scoped to the offline account SID, and offline-wrapper launch for no-network or managed-domain sandbox policies.
