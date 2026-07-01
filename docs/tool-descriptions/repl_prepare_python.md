`repl_prepare` changes or explicitly realizes the Python environment for later
`repl` calls.

Do not call `repl_prepare` preemptively. The Python `repl` tool is
self-starting, self-healing, and available for immediate code evaluation. Call
`repl_prepare` only when you intentionally need to change the default managed
requirements, request a Python version, select an existing Python executable or
venv, or explicitly realize the current managed requirements manifest.

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

Omit all arguments only when you explicitly want to realize the current managed
manifest without changing it. This is not required before normal `repl` use. The
result reports whether the session was unchanged or restarted and echoes the
full managed requirements manifest.
