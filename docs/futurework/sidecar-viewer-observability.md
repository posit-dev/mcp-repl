# Sidecar Viewer Observability

## Motivation

`mcp-repl` can return text and images through MCP, but the MCP client controls
how those results are presented. The server cannot force a plot to be displayed
prominently, cannot expose a live transcript panel inside the client, and cannot
request user permission escalations through the client UI.

A sidecar viewer is the main future path for giving humans more observability
into autonomous REPL work without changing the MCP protocol.

## Product Boundary

`mcp-repl` is primarily for an autonomous agent working in a long-lived R or
Python runtime. Posit Assistant is the better fit for a human and agent sharing
the same interactive R session. Both workflows are useful, but they optimize for
different interaction models.

The sidecar should not try to turn `mcp-repl` into an IDE-embedded shared
session. Its job is to let a human inspect what the agent ran and saw.

## Target Scenarios

### Live Transcript Inspection

Minimal task:

1. Start `mcp-repl` with sidecar viewing enabled.
2. The agent runs several `repl` calls.
3. A human opens a local sidecar URL and sees the executed input, stdout,
   stderr, status notices, and reset boundaries in order.

### Plot And Image Inspection

Minimal task:

1. The agent creates one or more plots from R or Python.
2. The MCP response still returns image content normally.
3. The sidecar shows the latest plot and retained image history without relying
   on the MCP client to display every image inline.

### Bundle And History Inspection

Minimal task:

1. A `repl` call produces oversized output.
2. `mcp-repl` writes the normal server-owned output bundle.
3. The sidecar links to or renders the same retained history files so the human
   can inspect the full transcript and image history.

## Constraints

- Keep the first sidecar read-only. Do not add a hidden control channel for
  running code or broadening permissions.
- Bind locally by default and require explicit configuration for any non-local
  access.
- Do not treat the sidecar as a permission prompt. Network and filesystem
  approvals still belong to client config, shell/edit approval flows, or a
  separately designed approval surface.
- Reuse server-owned output history where possible. Do not create a second,
  divergent transcript model.

## Acceptance Shape

- A sidecar-enabled run exposes a local URL only when requested.
- The visible timeline matches normal MCP replies for text, images, resets, and
  oversized-output bundles.
- Image history remains bounded by the same retention policy as output bundles.
- The sidecar can be disabled without changing normal MCP tool behavior.

## Non-Goals

- Replacing Posit Assistant's shared human/agent R session.
- Forcing MCP clients to render images differently.
- A general-purpose web IDE or terminal emulator.
