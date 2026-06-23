# Releasing

Release only from an immutable semver tag. No rolling dev release, moving dev
tag, or backfill workflow is part of the supported release process. No backfill
workflow exists for repairing old tags.

## Checklist

1. Update `Cargo.toml` to the release version.
2. Run the required local checks from `AGENTS.md`.
3. Create and push the matching tag with `git tag vX.Y.Z` and
   `git push origin vX.Y.Z`:

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

Pushing the tag starts `.github/workflows/release.yml`. The workflow validates
that the tag matches the Cargo package version, runs the release check matrix,
builds GitHub Release archives, builds PyPI wheels, smoke-tests installed
wheels, publishes `posit-mcp-repl` to PyPI, and creates or updates the GitHub
release.

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
