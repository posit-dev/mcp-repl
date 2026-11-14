# Open work

Keep unfinished work here, with enough context to find the owning code and decide what to do next.
Check the source and tests before starting an item, add an issue link when work is selected, and delete the entry when it is resolved.
The IDs are stable references, not priorities or release commitments.
Recurring release checks belong in [Release](../RELEASE.md).

## Runtime and resource limits

- [ ] **RT-1 — Resolve or bound process-environment changes during late R startup.**
  R can initialize after Python and its native services are running.
  [R integration](../src/worker/r_integration.rs) still sets library environment variables before initialization, and R's bootstrap can change the process environment.
  Establish which changes can race with native readers and choose a safe embedding interface or an explicit restriction.
  Verify startup-package behavior, existing Python objects and SQL connections, and reentrant callbacks.
  Separate runtime threads would not remove this process-wide risk.

- [ ] **RT-2 — Decide limits for internal frames and queued stdin.**
  The [worker transport](WORKER_PROTOCOL.md#launch-and-transport) has no general frame-size or stdin-queue cap.
  Public output budgets do not bound these allocations.
  Define rejection, streaming or backpressure, and cancellation behavior before adding limits.

- [ ] **RT-3 — Define recording retention and retrieval policy.**
  [Recordings](RECORDING_REFERENCE.md) have per-output limits but no aggregate disk quota, automatic cleanup, built-in read tool, or redaction.
  Select the required retention and retrieval behavior; protect active sessions and user-managed files.

- [ ] **RT-4 — Replace the R DuckDB Arrow adapter when incremental data-frame fetching is supported.**
  The temporary adapter lives in [the SQL bridge](../src/sql/bridge.R).
  Check supported DuckDB R versions for [upstream streaming support](https://github.com/duckdb/duckdb-r/pull/2587), then use ordinary DBI retrieval and remove Arrow-specific retained-result handling.
  Preserve data-frame results, row limits, previews, and cleanup; the scratch catalog also serves exact preview formatting.

- [ ] **RT-5 — Reproduce and fix interrupt completion during R DuckDB 2.0 Arrow fetching.**
  A query can remain active after an interrupt while its Arrow result is fetched.
  Exercise this through Console's SQL API and verify interruption and retained connection state before advancing the DuckDB fixture pin in [shared R support](../tests/support/r.py).
  Those tests pin DuckDB 1.5.6; production package selection is independent of that fixture.

- [ ] **RT-6 — Bound the remaining waits on the recording writer.**
  Shutdown finalization and the integrator's `recording` request wait for the [recording writer](../src/transcript.rs) without a limit, so a filesystem call that never returns keeps the process alive after MCP input closes.
  Before adding a deadline, decide what abandoned finalization leaves behind (writable files, no `recording_ended` event, incomplete projections) and how Console reports it.

## Sandbox and platforms

- [ ] **SP-1 — Verify the macOS policy exceptions against their callers.**
  Review each application exception against the sandboxed workflow and selected companion pin.
  Keep real-library regressions where distinct APIs exercise distinct permissions; resolver success alone does not establish worker permissions.
  Start with [policy ownership](ARCHITECTURE.md#policy-extensions-and-compatibility).

- [ ] **SP-2 — Assess managed UDP and path-specific Unix-socket support.**
  Console rejects `tcp_udp` on macOS/Linux and path-specific Unix-socket lists on Linux.
  Check the pinned companion's routing and executable behavior before changing admission or the [capability reference](SANDBOX_CONFIGURATION.md#native-capabilities).
  Schema support alone does not establish usable routing.

- [ ] **SP-3 — Port Windows fixture-only exclusions.**
  Use full-suite capability diagnostics to find shell/shebang, FIFO, and Unix-layout fixtures for otherwise supported behavior.
  Port those fixtures while retaining genuinely platform-specific cases on their owning OS.
  See [Windows validation](WINDOWS_DEVELOPMENT.md#testing-parity-and-remaining-gaps); native acceptance does not cover every skipped shared case.

- [ ] **SP-4 — Revisit the Python 3.14 test-runner pin.**
  Move development and CI to Python 3.15 when the DuckDB versions required by the SQL fixtures have compatible wheels.
  Verify the complete suite without requiring provider builds from source.
  This pin concerns development checks, not the versions Console can embed.

## Documentation and agent usability

- [ ] **DX-1 — Evaluate tool-description changes with agents.**
  Compare discovery, language choice, polling and input, dependency preparation, recovery, and token cost on the same scenarios.
  Use the [tool-description owners](TOOL_DESCRIPTIONS.md); shorter prose alone does not establish better behavior.

- [ ] **DX-4 — Evaluate explicit context actions in the Shiny fixture.**
  Consider an “Ask about this object” action that prefills the composer, and links from plots or tables to their producing evaluation.
  Keep context handoff deliberate: browsing or filtering must not add private queries or rows to the model's context.
  Interactive plots and redraw-at-size need a runtime output contract beyond captured images.
  See the [fixture](../tests/fixtures/control_shiny/README.md).

## Design questions

- [ ] **DQ-1 — Decide whether project trust needs a separate policy.**
  Consider an opt-in Git-aware write policy only alongside an explicit project-configuration trust model.
  Git presence alone must not authorize configuration or workspace writes.
  Current behavior is described in [configuration discovery](CONFIGURATION.md) and [sandbox profiles](SANDBOX_CONFIGURATION.md#profiles-and-temporary-storage).
