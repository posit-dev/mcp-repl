# Claude Clear Binding Simplification Design

## Goal

Replace the current Claude `/clear` session-binding heuristics with a small, sound contract:

- a server process binds to at most one Claude session for its lifetime
- `/clear` only restarts workers bound to that exact Claude session
- concurrent Claude sessions in the same working directory do not reset each other's workers
- Claude subagents sharing the same MCP connection do not implicitly restart the parent session's REPL

## Problem

The current design tries to infer worker ownership from multiple partially authoritative sources and then repair mismatches after the fact. That has produced repeated review findings around rebinding, cross-session resets, stale markers, startup races, and same-project concurrency.

The root issue is that the code treats session ownership as a recoverable heuristic instead of a stable binding.

## Small Contract

### Supported behavior

1. Claude clear binding is enabled only when `CLAUDE_ENV_FILE` is available and contains a parseable `MCP_REPL_CLAUDE_SESSION_ID` export.
2. A server process may bind at startup or bind late on a later request once the env file becomes readable.
3. Once bound, the server process stays bound to that `(env_file_path, session_id)` identity for the rest of its lifetime.
4. `SessionEnd(clear)` restarts only workers whose bound `(env_file_path, session_id)` exactly matches the hook context.
5. Multiple Claude sessions in the same working directory are supported as long as they have distinct `CLAUDE_ENV_FILE` paths.

### Explicit non-goals

1. Rebinding a worker from one Claude session to another.
2. Reusing a worker across Claude sessions, even in the same working directory.
3. Inferring ownership from `CLAUDE_PROJECT_DIR`.
4. Project-wide handoff logic.
5. Shared-`CLAUDE_ENV_FILE` session rollover behavior.
6. Recovering ownership from stale active markers or previous-session aliases.

## Authoritative Identity

The only authoritative binding source is the env file path plus the session id currently exported in that file.

- `CLAUDE_ENV_FILE` is required for Claude clear binding.
- The binding session id is read from the env file, not inferred from working directory state.
- When the env file contains multiple `MCP_REPL_CLAUDE_SESSION_ID` exports, the last valid export wins.
- Env file matching uses the exact path string captured from `CLAUDE_ENV_FILE`; no project-based or canonicalized-path fallback is part of the contract.
- `CLAUDE_PROJECT_DIR` may remain available for unrelated behavior, but it does not participate in Claude clear ownership.

This is the key simplification that makes same-directory concurrent sessions sound: the working directory is not a unique session key, while the env file path is.

## State Model

Each server process is in exactly one of these states:

1. `Unbound`
   - No authoritative Claude session is currently available.
   - No Claude-owned worker record exists.

2. `Bound(session_id, env_file_path)`
   - The process has a fixed Claude owner.
   - The instance record stores that exact owner.

3. `LateBoundPendingRestart(session_id, env_file_path)`
   - The process discovered its first authoritative session after the worker may already have served requests.
   - The instance record is written immediately when late binding succeeds.
   - Before serving the first bound request, the worker must restart once.
   - After that restart, the process moves to `Bound`.

Allowed transitions:

- `Unbound -> Bound` when startup registration finds a valid env file session.
- `Unbound -> LateBoundPendingRestart` when late registration discovers the first valid env file session.
- `LateBoundPendingRestart -> Bound` after the forced restart succeeds.

No transition from one bound session to another is allowed.

## Hook Semantics

### `SessionStart`

`SessionStart` becomes write-only:

1. Validate hook payload.
2. Append the current session id export to `CLAUDE_ENV_FILE`.
3. Readers resolve the active session by scanning the file from the end and taking the last valid `MCP_REPL_CLAUDE_SESSION_ID` export.
4. Do not scan existing worker records.
5. Do not rewrite ownership.
6. Do not maintain project-level or env-file-level activity ledgers.

`SessionStart` is no longer a rebinding event.

### `SessionEnd(clear)`

`SessionEnd(clear)` becomes an exact-session restart request:

1. Load instance records.
2. Read `CLAUDE_ENV_FILE` from the hook process.
3. Select records with `record.claude_session_id == hook.session_id` and `record.env_file_path == hook.CLAUDE_ENV_FILE`.
4. Queue a restart request for those records.

No previous-session matching is allowed.
If the hook-side `CLAUDE_ENV_FILE` is missing or unreadable, the hook is a no-op.

### `SessionEnd` with other reasons

No ownership maintenance is required. The hook may return after validating input because worker reuse across sessions is not supported.

## Server Behavior

### Startup bind

At server construction:

1. Attempt to read the current Claude session from `CLAUDE_ENV_FILE`.
2. If successful, create a bound instance record immediately.
3. If not successful, remain unbound.

### Late bind

Before serving a request:

1. If unbound, attempt the same env-file-based bind on every request until binding succeeds or the server exits.
2. If the bind succeeds late, write the bound instance record immediately.
3. Mark the worker for a one-time restart before serving the first bound request.
4. If the worker is still waiting for its initial inherited sandbox update, defer that restart until sandbox state is available.

This preserves the existing requirement that pre-binding interpreter state must not leak into the first Claude-bound request.

### After bind

After a server process is bound:

1. Ignore later Claude session changes.
2. Ignore later env file rewrites for different session ids.
3. Never transfer ownership to another Claude session.

This is the behavior that prevents a subagent `SessionStart` from resetting a parent session when both share the same MCP connection.

## Instance Record Format

Keep only the state needed for exact-session clear targeting and debugging:

- `claude_session_id`
- `env_file_path`
- `backend`
- `pid`
- `control_path`
- `started_unix_ms`

Remove:

- `previous_claude_session_id`
- `project_dir`
- `last_seen_unix_ms`
- project session records
- env file session activity records
- stale-session timeout logic

## Concurrency Guarantees

### Concurrent Claude sessions in the same working directory

Supported when each session has a distinct `CLAUDE_ENV_FILE`:

- session A binds to its own worker
- session B binds to its own worker
- `/clear` in A cannot restart B
- `/clear` in B cannot restart A

### Shared MCP connection with Claude subagents

If session A already owns the server process and Claude launches subagent session B on the same MCP connection:

- the server remains bound to A
- B does not steal ownership
- B does not trigger a restart of A's REPL through `SessionStart`
- `/clear` in B does not restart A's worker because matching is exact-session only

This is intentional. Shared-connection subagents are treated as a limitation of the environment, not a reason to reintroduce rebinding heuristics.

## Failure Handling

- If `CLAUDE_ENV_FILE` is missing, unreadable, or lacks a valid session export, Claude clear binding is disabled for that server until a later request can bind successfully.
- If a late bind cannot complete its forced restart because inherited sandbox state has not arrived yet, the restart remains deferred until sandbox state is available.
- If stale state files from the old design remain on disk, the new code ignores unsupported state instead of trying to reconcile it.

## Testing Strategy

Keep only invariant tests that match the reduced contract:

1. startup bind succeeds after `SessionStart` writes the env file
2. late bind restarts pre-bound worker state once
3. `/clear` restarts only exact-session workers
4. concurrent same-directory sessions with distinct env files do not reset each other
5. later `SessionStart` from another session does not steal or restart an already bound worker
6. late bind still cooperates with inherited sandbox initialization

Delete or rewrite tests that exercise removed behavior:

- project-state handoff
- previous-session matching
- stale active marker recovery
- shared-env-file session rollover
- rebinding after normal exits
- project-wide session selection

## Implementation Scope

Primary files:

- `src/claude.rs`
- `src/server.rs`
- `tests/claude_clear_binding.rs`
- `tests/sandbox_state_updates.rs`

Secondary file:

- `src/install.rs` only if the hook wiring can be simplified to match the reduced hook semantics

Ownership note:

- `src/claude.rs` owns the Claude binding state machine, record format, and hook behavior.
- `src/server.rs` only drives startup registration, late registration, and sync timing around worker execution and sandbox updates.

## Crash Residue

If a current-version instance record is left behind by a crashed server process:

- `/clear` may still enqueue a restart request into its control file
- no ownership recovery or reassignment is attempted
- later live servers ignore that stale record unless it exactly matches their own process-owned record path

## Success Criteria

The rewrite is successful when:

1. Claude clear ownership is defined by a single authoritative source.
2. No code path can transfer a bound worker from one Claude session to another.
3. `/clear` cannot reset another concurrent Claude session in the same working directory.
4. A subagent `SessionStart` on a shared MCP connection does not restart the existing REPL session.
5. The implementation and tests are materially smaller than the current heuristic design.
