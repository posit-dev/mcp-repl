# Managed Network Follow-Up

## Motivation

Managed networking should support common agent workflows without making the
worker's sandbox equivalent to open network access. Package installation is the
first workflow, but not the only one.

The important future tasks are:

- Install packages from configured repositories such as CRAN, PyPI, or an
  internal mirror while direct worker egress remains blocked.
- Let the runtime connect to an explicitly allowed database, for example an R
  `DBI::dbConnect()` call to `db.example.com:5432` or a local development
  database on `127.0.0.1:5432`.
- Let an agent iterate on a Shiny app from the runtime while a separate browser
  tool, such as Playwright, drives the app. The worker should be able to bind a
  local app port, the browser tool should be able to open that loopback URL, and
  unrelated worker network access should remain blocked.
- Let the runtime call an explicitly allowed local service, such as a local API
  used by tests or examples, without also allowing arbitrary remote network
  access.

This note is not a final design. It records the target scenarios, current
constraints, and implementation tradeoffs so the next slice can choose an
approach without rediscovering the same boundary conditions.

## Target Scenarios

### Package Repository Access

Minimal task:

1. Start `mcp-repl` with `workspace-write`, network access enabled, and allowed
   domains for a package repository.
2. The agent asks the runtime to install or resolve a package.
3. The worker can reach the configured repository through managed networking.
4. Direct worker socket egress to unrelated hosts remains blocked.

This is the current macOS happy path for HTTP(S)-aware package tooling.

### Database Access

Minimal task:

1. Start `mcp-repl` with a policy that allows a specific database endpoint such
   as `db.example.com:5432` or `127.0.0.1:5432`.
2. The agent asks the runtime to run a small DBI query.
3. The worker can connect to that endpoint and port.
4. The worker cannot connect to other database ports or unrelated hosts.

This needs a policy surface that can express TCP host+port intent. Treating all
database access as HTTP proxy traffic is the wrong abstraction.

### Shiny App Iteration

Minimal task:

1. The agent asks the runtime to start a Shiny app on an explicit loopback
   address and port, for example `127.0.0.1:3838`.
2. The server applies a sandbox policy that allows the worker to bind and serve
   that loopback port.
3. A browser tool outside the worker, such as Playwright, opens
   `http://127.0.0.1:3838`.
4. The agent edits code, restarts or refreshes the app, and inspects behavior in
   the browser tool.
5. The worker still cannot bind public interfaces or reach arbitrary remote
   hosts.

This needs local bind and inbound loopback permissions that are separate from
general local outbound connect permissions.

### Local Service Access

Minimal task:

1. Start `mcp-repl` with a policy that allows a specific loopback endpoint, for
   example `127.0.0.1:8000`.
2. A local service outside the worker is already listening on that endpoint.
3. The agent asks the runtime to call the local API from R or Python.
4. The worker can connect to that endpoint and port.
5. The worker cannot connect to other local services or unrelated remote hosts.

This needs explicit loopback connect permission. It is separate from Shiny app
iteration, where the worker binds a port and a browser tool connects to it.

## Current Shape

- The server starts a server-owned HTTP/SOCKS proxy when domain allow/deny rules
  are configured with network access enabled.
- The worker receives proxy environment variables for common HTTP, HTTPS, and
  SOCKS-aware tools.
- macOS Seatbelt permits outbound network traffic only to the proxy's loopback
  ports while managed networking is active.
- The proxy validates host/domain patterns before dialing upstream.
- Host matching supports exact hosts, `*.example.com`, and `**.example.com`.
- Exact URL/path filtering is intentionally unsupported because normal HTTPS
  proxying exposes only the `CONNECT host:port` target.

## Research Context

The current macOS slice and review loop established these boundaries. This
section keeps that reconnaissance visible for future work; it is context, not a
roadmap.

The direct-egress guarantees below are macOS/Seatbelt-specific. Future Linux or
Windows slices need equivalent OS-level enforcement before they can make the
same claims.

Already addressed in the current slice:

- Direct worker egress is blocked by Seatbelt while managed networking is
  active. Proxy-aware tools use the server-owned loopback proxy, while tools
  that ignore proxy environment variables still fail closed.
- Resolved upstream addresses are checked before dialing. Non-public upstreams,
  including IPv4-mapped IPv6 special ranges, are rejected except for loopback
  addresses when local binding is explicitly enabled.
- Plain HTTP proxy requests reject `Host` headers that do not match the checked
  absolute-form target.
- Plain HTTP proxy connections are treated as one-request streams, so a later
  request on a reused client connection cannot silently use the first checked
  upstream.

Open boundaries found during the same investigation:

- HTTPS `CONNECT` proves only the requested tunnel endpoint. It does not prove
  the TLS SNI or HTTP authority inside the tunnel.
- SOCKS support is a generic TCP tunnel. It helps compatibility for tools that
  only honor `ALL_PROXY`, but it does not provide HTTP URL/path visibility.
- Domain rules currently do not express port intent. Allowing a host allows
  every requested port on that host.
- Wildcards and package repositories are broad trust boundaries. Managed
  networking constrains worker egress; it does not make downloaded package code
  safe or turn macOS broad reads into a data-loss-prevention boundary.

## Design Constraints

- Managed package/web access, explicit TCP connect, and local app serving are
  different capabilities. They may need different policy fields and different
  enforcement paths.
- Tools that honor proxy environment variables should work transparently.
  Tools that ignore them must still be constrained by the OS sandbox.
- Local database connect does not imply local app bind. Local app bind does not
  imply remote network access.
- Specific URL policy entries should be documented carefully. For ordinary
  HTTPS traffic, the current proxy shape can derive host and optional port
  intent from a URL, but it cannot enforce path or query rules, and it cannot
  prove the bytes inside a `CONNECT` tunnel are HTTP for the requested scheme.
  Enforcing paths such as `https://pypi.org/simple/` requires TLS interception,
  a controlled mirror, or a package-manager-specific path.
- Allowing package repositories allows downloaded package code to execute
  inside the sandbox. Managed networking constrains egress; it does not make
  untrusted packages safe.
- On macOS, broad read permissions mean managed networking should not be treated
  as a data-loss-prevention boundary.

## Known Hardening Gaps

### CONNECT Host Does Not Prove TLS Destination

For HTTPS, the proxy checks the `CONNECT host:port` authority and then tunnels
bytes unchanged. The worker or a worker child process can ask the proxy to
connect to an allowlisted host while using TLS SNI and HTTP authority for a
different host served by the same edge infrastructure.

Example class:

- allowlist contains `pypi.org`,
- the worker or a worker child process sends `CONNECT pypi.org:443`,
- TLS ClientHello uses SNI `www.python.org`,
- the edge serves `www.python.org`.

This is not arbitrary internet access. It works when the allowlisted host's
resolved address or edge fabric can also serve the other hostname. That can
still be a large surface for domains hosted on shared CDNs.

Minimal R reproduction:

Start `mcp-repl` with managed networking enabled and only `pypi.org`
allowlisted. Then run this from the R runtime:

```r
library(curl)

h <- new_handle()
handle_setopt(
  h,
  connect_to = "www.python.org:443:pypi.org:443"
)

res <- curl_fetch_memory("https://www.python.org/", h)
body <- rawToChar(res$content)

stopifnot(res$status_code == 200L)
stopifnot(grepl("Python", body))
cat(substr(body, 1, 200))
```

The important detail is that the request URL and TLS SNI are
`www.python.org`, while `connect_to` makes the proxy tunnel target
`pypi.org:443`. If the current managed proxy only checks the `CONNECT` target,
the request can succeed even though `www.python.org` is not allowlisted.

This concrete `pypi.org` and `www.python.org` pair depends on edge routing that
may change. If it stops reproducing, the underlying class remains: the
`CONNECT` authority and the TLS SNI or HTTP authority inside the tunnel can
diverge unless the proxy checks them.

### SOCKS Is A Generic TCP Tunnel

The managed proxy exposes SOCKS so tools that only honor `ALL_PROXY` can work.
SOCKS is not HTTP-aware and does not give the proxy URL/path semantics. Without
additional protocol gates, any allowed host can become a generic TCP endpoint.

### Ports Are Not Currently Part Of The Domain Policy

Allowing a host allows connections to any requested port on that host. That is
broader than package-manager HTTPS needs, which is usually port 443.

### Domain Rules Are Coarse

Wildcard domains are intentionally convenient but broad. `**.example.com`
allows the apex and every subdomain, including future or compromised
subdomains. Domain rules also cannot distinguish package repository paths from
other services on the same host.

## Possible Directions

These are implementation options, not a prescribed roadmap.

- Add explicit TCP connect allowlists for database and service endpoints, for
  example `db.example.com:5432`.
- Split local loopback connect from local loopback bind/inbound permissions, so
  database workflows and Shiny workflows can be enabled independently.
- Restrict HTTP `CONNECT` to package-repository-shaped traffic, such as port
  443 by default, unless an explicit TCP policy grants a different port.
- Add TLS ClientHello SNI validation for `CONNECT` to reduce host/SNI mismatch
  risk without terminating TLS.
- Remove SOCKS from the default managed web path, or gate SOCKS traffic by port
  and protocol when compatibility requires it.
- Use controlled package mirrors when package governance needs path or package
  identity guarantees outside `mcp-repl`.
- Consider HTTPS MITM only if URL/path policy becomes a first-class requirement;
  certificate management and trust-store changes make that a much larger
  feature.

## Next Slice Guidance

Choose the next design by starting from one concrete task. Good first slices are:

- explicit TCP connect for one database endpoint,
- explicit loopback bind/inbound for Shiny app iteration,
- explicit loopback connect for one local service endpoint,
- TLS SNI gating for the existing package-repository proxy path.

Each slice should include a minimal end-to-end scenario that proves the intended
workflow works and that unrelated network access still fails closed.
