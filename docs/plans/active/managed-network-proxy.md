# Managed Network Proxy

## Summary

- Add a macOS-first managed network proxy for workers.
- Keep existing full-network sandbox behavior unchanged when no managed domain rules are configured.
- Keep Linux and Windows as explicit follow-up phases with clear unsupported errors for managed domain enforcement.

## Status

- State: active
- Last updated: 2026-05-01
- Current phase: Linux planning

## Current Direction

- Use a server-owned HTTP/SOCKS proxy, proxy environment variables injected into the worker, and macOS Seatbelt egress limited to the proxy's loopback ports.
- Use host/domain matching only for this slice. Exact URL/path filtering is out of scope because normal HTTPS proxying exposes only the `CONNECT host:port` target unless a MITM certificate flow is added.
- Keep the code small and local to `mcp-repl`: use a simple proxy runtime tailored to package-install workflows.

## Long-Term Direction

- Linux should route worker traffic through a server-owned proxy from inside the Linux sandbox without allowing direct egress.
- Windows should use the same policy surface once the Windows sandbox can route worker traffic through a managed proxy.
- A future UI or approval flow can amend allow/deny rules, but this phase only supports static CLI/config rules.

## Phase Status

- Phase 0: completed - chose the managed proxy shape and enforcement boundary.
- Phase 1: completed - implemented macOS managed proxy and public tests.
- Phase 2: pending - Linux enforcement.
- Phase 3: pending - Windows enforcement.

## Locked Decisions

- macOS enforcement uses Seatbelt loopback-only egress to the managed proxy ports.
- Domain policy is deny-first and allowlist-based.
- Supported patterns are exact hosts, `*.example.com`, and `**.example.com`.
- Exact URLs are rejected instead of being silently reduced to hosts.
- Proxy-aware tools are the transparency target; tools that ignore proxy env vars fail closed.

## Open Questions

- Which Linux routing mechanism should become the long-term implementation: the existing internal sandbox helper, a socket bridge, or a separate network namespace path.
- Whether a future HTTPS MITM mode is worth the certificate-management surface for URL/path-level filtering.
- Which hardening slice should land first for the current macOS proxy: TLS SNI gating for `CONNECT`, SOCKS removal/gating, explicit TCP allowlists, or split local connect/bind controls. See `docs/futurework/managed-network-hardening.md`.

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
- 2026-05-01: Documented current managed-network hardening gaps and mitigation tradeoffs after demonstrating that an allowlisted `CONNECT` host can be paired with a different TLS SNI on shared edge infrastructure.
