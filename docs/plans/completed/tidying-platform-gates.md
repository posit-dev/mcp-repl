# Tidying Platform Gates

## Summary

- Completed: 2026-06-26
- Removed platform timing branches that were compensating for races instead of OS-specific contracts.
- Kept true OS behavior gates, especially sandbox tests.
- Replaced fixed sleeps with observable public replies, file markers, or bounded polls.

## Decisions

- The PR 150 macOS CI failure was a completion-settle race after split UTF-8 output, not a Linux sandbox regression.
- Completion settling now uses one output-stability window across platforms.
- Tests that need slower process startup or filesystem behavior should wait on state, not choose longer sleeps for Windows.
