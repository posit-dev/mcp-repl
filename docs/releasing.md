# Releasing

Release only from an immutable semver tag. The only supported manual backfill is
the PyPI-only dispatch below, and it must target an existing immutable semver
tag. No rolling dev release, moving dev tag, or workflow for repairing old
GitHub Release assets is part of the supported release process.

## Checklist

1. Update `Cargo.toml` to the release version.
2. Run the required local checks from `AGENTS.md`.
3. Create and push the matching tag with `git tag vX.Y.Z` and
   `git push origin vX.Y.Z`:

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

Prerelease tags may use only PyPI-compatible lowercase `aN`, `bN`, or `rcN`
suffixes, for example `vX.Y.Z-rc1`.

Pushing the tag starts `.github/workflows/release.yml`. The workflow validates
that the tag matches the Cargo package version, runs the release check matrix,
builds GitHub Release archives, builds PyPI wheels, smoke-tests installed
wheels, publishes `posit-mcp-repl` to PyPI, and creates or updates the GitHub
release.

## Manual PyPI Backfill

Use this path only when a downstream plugin is blocked and `posit-mcp-repl`
must be published before the next scheduled release tag.

1. Confirm the target is an existing immutable semver tag whose version is not
   already on PyPI.
2. Open the `Release` workflow in GitHub Actions and run it manually.
3. Set `pypi_backfill_tag` to the existing tag, for example `vX.Y.Z`.

The manual dispatch checks out that tag, validates that the tag is final semver
or PyPI-compatible prerelease semver, validates that it matches `Cargo.toml`,
builds and smoke-tests the release binary and PyPI wheels, and then runs only
`publish-pypi`. It does not rerun historical validation checks; this path assumes
that the checks required at the time already passed when the tag was created.
For tags that predate PyPI packaging, the dispatch overlays the current
`pyproject.toml` packaging metadata only; the compiled source still comes from
the immutable tag.

The manual dispatch does not create or update the GitHub release, move tags,
publish a dev version, repair old GitHub Release assets, or upload an sdist. If
the version already exists on PyPI, publishing fails.

## PyPI Trusted Publisher

Before the first publish, configure a PyPI Trusted Publisher for:

- Owner: `posit-dev`
- Repository: `mcp-repl`
- Workflow: `release.yml`
- Environment: `pypi`
- Package: `posit-mcp-repl`

## PyPI Artifacts

PyPI publishing is Wheel-only. Wheels include the compiled `mcp-repl`
executable and do not bundle R or Python runtimes. R is optional: it is not
required to build the wheel, install the package, or use Python-backed
`mcp-repl` sessions. Users need R only when they choose the R interpreter.

The Linux PyPI wheel is built with manylinux2014. This keeps the wheel usable on
older supported Linux distributions without tying PyPI installs to the GitHub
Release archive's Ubuntu 22.04 glibc baseline.

Do not publish an sdist yet. An sdist is a source distribution that asks pip to
compile the project locally when no wheel is available. For this package that
would require a Rust toolchain and Cargo dependency resolution during install,
including current git dependencies. Wheel-only publishing makes unsupported
platforms fail clearly instead of attempting a slow or network-sensitive source
build.
