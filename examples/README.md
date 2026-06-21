# chatlas examples

These examples show how to register `mcp-repl` as a stdio MCP server from
`chatlas` with `register_mcp_tools_stdio_async`, expose the `repl` tool to the
model, and call `cleanup_mcp_tools` when the chat finishes. They focus on the
two oversized-output modes: pager mode and overflow files mode.

## Prerequisites

Install `mcp-repl` and make sure it is on `PATH`:

```sh
cargo install --git https://github.com/posit-dev/mcp-repl --locked
```

Install chatlas with MCP support:

```sh
pip install 'chatlas[mcp]'
```

Set credentials for the model provider used by `ChatOpenAI`, usually:

```sh
export OPENAI_API_KEY=...
```

The examples use the Python interpreter, so `mcp-repl` needs a loadable Python
runtime.

## Examples

- `chatlas_pager_mode.py`: starts `mcp-repl --oversized-output pager
  --interpreter python`, asks the model to produce oversized output, then uses
  pager commands such as search to inspect it.
- `chatlas_files_mode.py`: starts `mcp-repl --oversized-output files
  --interpreter python`, asks the model to produce output large enough to spill
  into an output bundle, then gives the model `list_directory` and
  `read_text_file` tools so it can inspect that bundle.
- `chatlas_sync_pager_mode.py`: defines a normal synchronous chatlas tool with
  `McpReplTool`, then uses `chat.chat()` with `mcp-repl --oversized-output pager
  --interpreter python`.
- `chatlas_sync_files_mode.py`: defines the same synchronous `repl` tool for
  `mcp-repl --oversized-output files --interpreter python`, plus the same
  bundle inspection tools used by the async files-mode example.
- `mcp_repl_tool.py`: small MCP stdio-backed tool used by the synchronous
  chatlas examples.

Run an async MCP-registration example from this repository:

```sh
python examples/chatlas_pager_mode.py
python examples/chatlas_files_mode.py
```

Both scripts register only the MCP `repl` tool, not `repl_reset`, so the model
gets the single happy path: run code in the live REPL and summarize the result.

The files-mode example also registers ordinary chatlas tools:

- `list_directory(path)`: lists direct children of a disclosed output bundle.
- `read_text_file(path, start_line, end_line)`: reads a UTF-8 text file with
  optional inclusive line ranges. Leave `end_line` unset to read through EOF.
  The tool has no artificial size limit; the range arguments are there so the
  model can choose how much of a large bundle file to inspect at a time.

## Synchronous chatlas examples

chatlas supports synchronous `chat.chat()` and `chat.stream()` calls for ordinary
chats and synchronous tools. Its `register_mcp_tools_stdio_async()` MCP
registration path is async, so the synchronous examples do not use that helper.
Instead, they define the regular tool object `McpReplTool`, which owns the
`mcp-repl` subprocess, speaks MCP over stdio, and can be registered directly
with `chat.register_tool(repl)`.

The synchronous examples use an explicit lifecycle:

```python
repl = McpReplTool([...])
try:
    repl.start()
    chat.register_tool(repl)
    chat.chat("...")
finally:
    repl.close()
```

Run a synchronous chatlas example from this repository:

```sh
python examples/chatlas_sync_pager_mode.py
python examples/chatlas_sync_files_mode.py
```

Use the async examples when you want chatlas to manage the MCP connection. Use
the sync examples when you need `chat.chat()` and are willing to let the example
tool own the MCP stdio process directly.
