# Examples

This folder contains small starter files for using `mcp-repl` directly.

## ellmer with pager overflow

[`examples/ellmer-mcp-repl.R`](ellmer-mcp-repl.R) shows how to use
`mcptools::mcp_tools` to expose `mcp-repl` tools to an `ellmer` chat.
`mcptools::mcp_tools()` currently takes a configuration file path, so the
example writes a temporary Claude-style JSON config before loading the tools.

Prerequisites:

- Install `mcp-repl` and make sure `mcp-repl` is on `PATH`, or set
  `MCP_REPL_BINARY` to the command or full path.
- Install the R packages used by the examples:

```r
install.packages(c("ellmer", "mcptools", "jsonlite", "glue"))
```

- Set model credentials for your selected `ellmer` provider. The script uses
  `ellmer::chat_openai`, so the default path expects `OPENAI_API_KEY`.

Run it with:

```sh
Rscript examples/ellmer-mcp-repl.R
```

The script starts `mcp-repl` as a local MCP server with `--interpreter r`,
loads the `repl` and `repl_reset` tools through `mcptools`, and registers
those tools on an `ellmer` chat. It uses `--oversized-output pager` so large
REPL output can be paged through the same `repl` tool instead of returning
bundle paths that would require a separate file-reading tool.

## ellmer with files overflow

[`examples/ellmer-mcp-repl-files.R`](ellmer-mcp-repl-files.R) uses
`--oversized-output files`. In this mode, large replies may return an output
bundle path. The example registers extra `list_directory` and `read_text_file`
tools so the model can discover bundle contents and inspect text files in that
bundle, especially `transcript.txt` and `events.log` when it is present. The
directory listing includes `type`, `size`, and `path` columns.
`read_text_file` reads a bounded line window, decodes text as UTF-8, and shows
invalid bytes with byte escapes. When output is truncated, the tool returns the
next `start_line` value to use for the following read. Returned text includes
line numbers by default.

Ordinary workspace files can still be read by running R code such as
`list.files()`, `readLines()`, or `read.csv()` through `repl`. The extra file
tools are for server-owned output bundle paths returned by files mode. The model
gets a separate persistent `mcp-repl` R session; it is not the same R process
that runs the script.
