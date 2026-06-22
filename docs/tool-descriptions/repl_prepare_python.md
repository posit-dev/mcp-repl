`repl_prepare` prepares the Python REPL for later `repl` calls.

mcp-repl maintains a persistent managed-Python requirements manifest, initially containing `numpy`; `uv` realizes that manifest into an ephemeral environment. Use `requirements` to add, remove, or set manifest entries. By default, mcp-repl restarts only if needed to satisfy the manifest. Set `restart: "no"` to preserve current session state, or `restart: "yes"` to force a fresh session. Use `python` to select an explicit Python executable or venv instead of the ephemeral environment produced from the manifest.

Arguments:

- `requirements`: update and realize the managed requirements manifest.
  - `packages`: optional array of Python distribution requirements.
  - `python_version`: optional uv-supported Python version request or constraint, forwarded as supplied.
  - `action`: optional `"add"`, `"remove"`, or `"set"` manifest operation. Defaults to `"add"`.
  - `restart`: optional `"if_needed"`, `"no"`, or `"yes"`. Defaults to `"if_needed"`.
- `python`: use an existing Python as-is.
  - `executable`: absolute path to a Python executable.
  - `venv`: absolute path to a Python virtual environment directory. The executable is `bin/python` on Unix and `Scripts/python.exe` on Windows.

Provide either `requirements` or `python`, not both. Inside `python`, provide exactly one of `executable` or `venv`. `restart` applies only inside `requirements`.

Omit all arguments to realize the current manifest. The result reports whether the session was unchanged or restarted and echoes the full managed requirements manifest.
