# chatlas examples

These scripts use `uv run --script`. Install `mcp-repl`, set model
credentials for `ChatOpenAI`, then run:

The helpers set `MCP_REPL_PYTHON_EXECUTABLE=sys.executable`, so the REPL tool
uses the same Python installation as the chatlas script.

```bash
cargo install --git https://github.com/posit-dev/mcp-repl --locked
export OPENAI_API_KEY=...
uv run --script examples/chatlas/chatlas_async_pager_mode.py
uv run --script examples/chatlas/chatlas_async_files_mode.py
uv run --script examples/chatlas/chatlas_pager_mode.py
uv run --script examples/chatlas/chatlas_files_mode.py
```

Both examples ask:

```text
Tell me something interesting about the penguins dataset. Use the REPL tool to do analysis.
```

- `chatlas_async_pager_mode.py`: pager mode with the `repl` MCP tool.
- `chatlas_async_files_mode.py`: overflow files mode with `repl`,
  `list_directory`, and `read_text_file`.
- `chatlas_pager_mode.py`: pager mode with ordinary chatlas tools.
- `chatlas_files_mode.py`: overflow files mode with ordinary chatlas tools.
- `chatlas_tools.py`: `register_tool_repl(chat, overflow=OverflowMode...)` and
  shared helper tools.
