# Future Work: Per-Turn History Bundles

## Summary

Potential follow-on design: keep a server-owned history bundle for every accepted turn so
agents can inspect recent REPL history through a stable filesystem layout instead of only
through lazily disclosed oversized-output bundles.

Recommended direction:

- create one turn directory for every accepted turn
- keep `last-turn` as a stable symlink to the newest turn
- prune old turns automatically, with a default count cap of 20 plus byte caps
- keep bundle creation synchronous in the first implementation

This should be treated as a design and implementation brief for a future agent. It is not
the current repository contract.

## Why

Current behavior only materializes bundle files when a reply times out, spills over the hard
text threshold, or needs mixed text/image indexing. That keeps small replies cheap, but it
also means:

- small turns leave no inspectable history bundle
- timeout handling needs hidden-vs-disclosed bundle state
- a future agent cannot rely on one stable place to look back at recent turns

Per-turn bundles improve utility and simplify some of the current response logic:

- every accepted turn is inspectable
- timeout follow-up polls can append to an already-existing turn bundle
- the response layer no longer needs to decide whether files exist, only how much to show inline

## Desired Outcomes

- A future agent can inspect recent REPL history by reading one stable session-history root.
- The newest turn is always reachable through `last-turn`.
- Polls do not create extra turn directories.
- Timeout continuation appends to the same turn directory.
- The visible inline reply behavior stays bounded and user-friendly.
- The tool descriptions stay stable; the runtime history path is disclosed by normal reply text.

## Turn Semantics

Turn boundaries are based on accepted top-level actions, not on every tool call and not on
internal recursion.

Count as a new turn:

- a non-empty top-level `repl(input=...)`
- a bare top-level `interrupt`
- a bare top-level `restart`
- `repl_reset`

Do not count as a new turn:

- empty-input polls
- timeout continuation polls
- idle polls that only return `<<repl status: idle>>`

Important rule for control prefixes:

- a non-empty top-level input that begins with `\u0003` or `\u0004` is still one turn
- `\u0003foo` is one turn that starts with interrupt handling and ends with the reply for evaluating `foo`
- `\u0004foo` is one turn that starts with restart handling and ends with the reply for evaluating `foo`

This matters because the current worker path recursively re-enters `write_stdin()` after handling
the control prefix. The turn layer must not mirror that recursion into a second turn.

## Session Root

Do not store turn history under the worker session temp dir. That directory is recreated on
worker spawn/reset, so it has the wrong lifetime for server-owned history.

Preferred root selection:

- if a debug session dir exists, use `<debug-session-dir>/turns`
- otherwise allocate a server-owned temp root for the lifetime of the MCP server

The root should be cleaned up when the server exits.

## Turn Layout

Recommended per-turn layout:

```text
turn-0001/
  events.log
  transcript.txt
  images/
  images/history/
```

Recommended root layout:

```text
session-history/
  last-turn -> turn-0007
  turn-0001/
  turn-0002/
  ...
```

File rules:

- `events.log` always exists and is the authoritative ordered index for the full normalized visible stream
- `transcript.txt` stores worker-originated REPL text only
- `transcript.txt` should be created eagerly, even if it stays empty
- server-only notices do not go into `transcript.txt`
- `images/` and `images/history/` are only created when the turn emitted images
- top-level files under `images/` represent the latest image state that matches the collapsed inline reply
- `images/history/` preserves the full ordered image history for the turn, including same-turn plot updates that are intentionally collapsed out of standard inline REPL output

Recommended `events.log` row types:

- `T`: worker text range in `transcript.txt`
- `S`: server-only text
- `I`: image history path

This keeps turn inspection uniform for text-only, mixed, timeout, reset, and interrupt flows.
It also preserves more image history than the default inline reply is expected to show.

## Inline Reply Behavior

Do not change the main public interaction model:

- small replies stay inline
- oversized replies still use bounded previews
- image-heavy replies still keep the inline anchors that are useful in the current UX

The change is only that file materialization becomes unconditional per turn. Inline compaction
still decides what the client sees directly.

Important image rule:

- standard inline REPL output may intentionally collapse same-turn plot updates to the final visible image state
- the turn bundle must preserve the full image history anyway
- an agent that needs the full plot-update history should be able to inspect `events.log` plus `images/history/` and recover more than the collapsed inline reply shows

Preferred discoverability:

- do not make the tool description dynamic
- emit one short server note on the first accepted turn that discloses the history root
- after that, the client can use `last-turn` or inspect older `turn-*` directories directly

Dynamic tool descriptions are possible, but they add surface area for little value and make the
description less stable.

## Retention And Pruning

Recommended defaults:

- keep the last 20 completed turns
- also keep the current active turn, even if that means 21 directories temporarily
- retain byte caps in addition to the count cap

Pruning rules:

- prune only inactive turns
- update `last-turn` atomically with a symlink swap
- keep cleanup on server shutdown

The current oversized-output bundle defaults already use a count cap of 20. Reusing that default
for turn history is reasonable.

## Latency Expectations

Turn history creation will be on the response critical path unless a future implementation adds a
background writer.

Measured local order-of-magnitude costs on this machine were small:

- directory only: about `0.05 ms` average
- empty `transcript.txt` plus small metadata file: about `0.19 ms` average
- 4 KB transcript plus `last-turn` symlink update: about `0.26 ms` average
- 64 KB transcript plus `events.log` plus `last-turn`: about `0.34 ms` average
- 1 MB image decode plus two image writes: about `1.4 ms` average

These numbers are optimistic because they do not include forced fsyncs and they benefit from the
local temp filesystem cache. Still, they are small enough that the first implementation should stay
synchronous.

Do not optimize this with async bundle creation first. Async writing introduces ordering and
visibility races with:

- timeout follow-up polls
- same-turn appends
- immediate inspection of `last-turn`

If a future implementation wants more concurrency, it should start from a correct synchronous design
and only then move to a dedicated ordered writer.

## Recommended Refactor Shape

Replace the current lazy bundle-specific state with turn-specific state.

Recommended model:

- `ResponseState` owns a turn store rooted once per server session
- the active turn exists as soon as a new accepted turn starts
- reply finalization always appends normalized items into the active turn
- reply presentation decides only whether to show everything inline or a bounded preview

This should remove or simplify:

- hidden-vs-disclosed bundle state
- timeout-specific bundle setup branches
- lazy `events.log` materialization paths

It should preserve:

- worker-vs-server text separation
- timeout follow-up polling semantics
- current bounded preview behavior

## Test Cases

Public behavior to cover:

- under-threshold text reply creates `turn-0001`, `events.log`, `transcript.txt`, and `last-turn`
- multiple timeout polls append to one turn and do not create extra turns
- idle polls do not create turns
- detached idle output remains non-blocking and is only persisted when attached to a later accepted turn
- bare `\u0003`, bare `\u0004`, and `repl_reset` each create one new turn
- `\u0003foo` creates one turn, not two
- `\u0004foo` creates one turn, not two
- mixed text/image replies always write `events.log` plus image history
- same-turn plot updates remain collapsed inline but are fully preserved under `images/history/`
- pruning removes the oldest inactive turn after the retention cap is exceeded
- cleanup removes the session-history root on server exit

## Non-Goals

Do not treat these as part of the first implementation:

- dynamic tool descriptions that include the current history path
- background or async turn writers
- a new MCP read-history tool
- changing the existing inline preview contract beyond what is required to mention the history root once

## Relevant Current Files

- `src/server/response.rs`: current lazy output-bundle store and reply finalization
- `src/worker_process.rs`: accepted-input vs poll semantics and control-prefix handling
- `src/debug_logs.rs`: optional per-session debug directory
- `src/sandbox.rs`: worker session temp dir lifecycle, which should not be reused for turn history
