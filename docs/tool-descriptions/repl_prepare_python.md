`repl_prepare` prepares the Python REPL runtime for later `repl` calls.

Use this when later Python code requires packages or a specific existing Python.
It does not run user code, import packages, attach modules, or create aliases.

Arguments:

- Omit all arguments to prepare an ephemeral uv-managed Python with the default packages: `numpy`.
- `requirements`: prepare an ephemeral uv-managed Python.
  - `packages`: optional exact array of Python distribution requirements. Supplying this replaces defaults; it does not add to them.
  - `python_version`: optional uv-supported Python version request or constraint, forwarded as supplied.
- `python`: use an existing Python as-is.
  - `executable`: absolute path to a Python executable.
  - `venv`: absolute path to a Python virtual environment directory. The executable is `bin/python` on Unix and `Scripts/python.exe` on Windows.

Provide either `requirements` or `python`, not both. Inside `python`, provide exactly one of `executable` or `venv`.

`requirements: {}` means stdlib-only ephemeral Python. Explicit `python` selections are non-mutating and receive no default packages.

Preparation may preserve or replace the current session. If replaced, previous variables, imports, pending output, and active work are lost. The reply states whether the session was unchanged or replaced and whether pending work was discarded.
