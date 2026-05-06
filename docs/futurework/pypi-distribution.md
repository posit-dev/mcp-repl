# PyPI Distribution

## Motivation

Some users expect MCP servers to be installable through Python packaging tools
such as `pip`, `pipx`, or `uvx`. `mcp-repl` already has Cargo installs and
GitHub-hosted prebuilt binaries; a PyPI package would be an additional
distribution surface for users whose MCP setup is Python-package oriented.

## Relationship To Current Releases

- The current source install path is `cargo install --git ... --locked`.
- The current binary install path is GitHub Releases plus the shell and
  PowerShell installers.
- A PyPI package should not replace either path until it has equivalent
  platform coverage and smoke testing.
- The package must not bundle R or Python runtimes. It should only provide the
  `mcp-repl` server executable or a thin launcher for it.

## Possible Shapes

- Platform wheels that contain the compiled `mcp-repl` binary.
- A small Python launcher package that downloads a matching GitHub Release
  artifact at install or first run.
- A source-build package that shells out to Cargo during build.

Platform wheels are the best user experience once release automation supports
them. A downloader package is simpler but needs careful checksum, version, and
offline behavior. A source-build package is the least friendly default because
it requires a Rust toolchain.

## Design Constraints

- Keep versions aligned with Cargo and GitHub Release tags.
- Verify downloaded or embedded binaries before exposing them on `PATH`.
- Make `uvx mcp-repl` and `pipx install mcp-repl` realistic target workflows if
  the package exposes a Python entry point.
- Avoid hidden network access during normal `mcp-repl` execution. Any download
  behavior belongs to installation or an explicit update command.
- Keep installer docs clear about which path is authoritative for each release
  channel.

## Acceptance Shape

- CI builds or validates PyPI artifacts for every supported platform.
- A clean environment can install from the package and run `mcp-repl --help`.
- `mcp-repl install --client codex` and `mcp-repl install --client claude` work
  when the executable came from the PyPI package.
- Release docs explain Cargo, GitHub binary, and PyPI install paths without
  implying that one silently configures the others.

## Open Questions

- Should PyPI publish all release channels or only stable semver releases?
- Should dev builds be available through a separate package name or avoided on
  PyPI?
- Is the package name `mcp-repl` available and appropriate?
- Should plugin packaging prefer `uvx mcp-repl` once this exists?
