# Output Ring Test Isolation

The unit suite still relies on one process-global output ring. `WorkerManager::new`
resets that ring, so parallel unit tests that create managers can interfere with
other tests that are asserting pager/output-ring behavior.

Current mitigation:

- keep pager/output-ring unit coverage on deterministic lower-level helpers when
  possible, instead of asserting through shared global ring state.

Future work:

- make the output ring injectable or per-manager in tests so pager input-context
  coverage can exercise `WorkerManager` directly without cross-test interference.
- audit remaining output-ring tests and remove any dependence on implicit global
  resets from unrelated manager construction.
