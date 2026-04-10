# Rolling Dev Binary Release

## Summary

- Add a rolling published prerelease named `dev` on GitHub Releases.
- Add stable binary publishing for semver tags without changing how semver releases are named.
- Keep GitHub Actions artifacts as an internal handoff only and use GitHub Releases for user downloads.

## Status

- State: active
- Last updated: 2026-04-08
- Current phase: implementation

## Current Direction

- Keep the existing CI workflow as the single entrypoint for PR validation, `main` dev publishing, and tag-based stable publishing.
- Add release-mode packaging to every existing CI matrix job so packaging failures are visible on PRs, `main`, and stable tag pushes.
- Lock the public download contract in `README.md`, small installer scripts, and docs contract tests so stable filenames and URLs stay intentional.

## Long-Term Direction

- Keep `dev` as the mutable prerelease channel and keep stable semver releases as immutable user-facing milestones.
- If the project later needs broader platform coverage or richer installers, add them without changing the meaning of either release channel.

## Phase Status

- Phase 0: completed
- Phase 1: active
- Phase 2: pending

## Locked Decisions

- The public download surface is GitHub Releases, not Actions artifacts.
- The `dev` channel is a published prerelease and must not become the repository's latest stable release.
- v1 covers Linux x86_64 glibc on Ubuntu 22.04, macOS arm64, and Windows x86_64 with stable asset names.
- Keep release orchestration in the existing workflow and prefer `gh` over a third-party release action.

## Open Questions

- None for this implementation slice.

## Next Safe Slice

- Add stable tag publishing and installer scripts that target the same archive layout already used for the `dev` channel.

## Stop Conditions

- Stop and update the plan if the existing workflow structure cannot support the publish gating without a separate workflow.
- Stop and update the plan if GitHub-hosted runner assumptions for any required platform change during implementation.

## Decision Log

- 2026-04-08: Chose a single-workflow design so the existing PR/main validation path and the release paths stay coupled.
- 2026-04-08: Chose a docs contract test as the first red-green slice because the public change is distribution-facing rather than a Rust API change.
- 2026-04-08: Added tag-based stable publishing and installer scripts rather than switching to a separate release action.
