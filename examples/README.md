# chatlas examples

These examples show the chatlas-supported MCP path for `mcp-repl`:
`register_mcp_tools_stdio_async()`.

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

- `chatlas_async_pager_mode.py`: registers only the MCP `repl` tool in pager mode
  with `mcp-repl --oversized-output pager --interpreter python`.
- `chatlas_async_files_mode.py`: registers the MCP `repl` tool with `mcp-repl
  --oversized-output files --interpreter python`, plus `list_directory` and
  `read_text_file` from `chatlas_file_tools.py`.

Both examples ask the same prompt:

```text
Tell me something interesting about the penguins dataset. Use the REPL tool to do analysis.
```

Run an async MCP-registration example from this repository:

```sh
python examples/chatlas_async_pager_mode.py
python examples/chatlas_async_files_mode.py
```

Both scripts register only the MCP `repl` tool, not `repl_reset`, so the model
gets the single happy path: run code in the live REPL and summarize the result.
They call `cleanup_mcp_tools` when the chat finishes.

The overflow files mode example also registers ordinary chatlas tools:

- `list_directory(path)`: lists direct children of a disclosed output bundle.
- `read_text_file(path, start_line, end_line)`: reads a UTF-8 text file with
  optional inclusive line ranges. Leave `end_line` unset to read through EOF.
  The tool has no artificial size limit; the range arguments are there so the
  model can choose how much of a large bundle file to inspect at a time.

chatlas supports blocking `chat.chat()` and `chat.stream()` calls for ordinary
chats and tools, but it does not currently expose a synchronous MCP registration
helper. For that reason these examples do not include a `chat.chat()` MCP
example. They use `chat_async()` so chatlas owns the MCP subprocess and protocol
handling through `register_mcp_tools_stdio_async()`.
