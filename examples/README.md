# Examples

This folder contains small starter files for using `mcp-repl` from `ellmer`.

The runnable scripts are the files to copy first. Shared setup lives in
[`examples/ellmer-mcp-repl-helpers.R`](ellmer-mcp-repl-helpers.R) so the scripts
stay focused on the shape of the `ellmer` workflow.

Prerequisites:

- Install `mcp-repl` and make sure `mcp-repl` is on `PATH`, or set
  `MCP_REPL_BINARY` to the command or full path.
- Select the R installation used by the `mcp-repl` worker. The examples use the
  current `R.home()` by default, or set `MCP_REPL_R_HOME` to another R home.
- Install the R packages:

```r
install.packages(c("ellmer", "mcptools", "jsonlite", "glue"))
Sys.setenv(MCP_REPL_R_HOME = R.home())
```

- Set credentials for the provider used by `ellmer::chat_openai`:

```r
Sys.setenv(OPENAI_API_KEY = "...")
```

## Pager Overflow

[`examples/ellmer-mcp-repl.R`](ellmer-mcp-repl.R) starts `mcp-repl` with
`--oversized-output pager` and registers `repl_tools(overflow = "pager")` on an
`ellmer` chat.

Run it with:

```sh
Rscript examples/ellmer-mcp-repl.R
```

## Files Overflow

[`examples/ellmer-mcp-repl-files.R`](ellmer-mcp-repl-files.R) uses
`--oversized-output files`. Large replies may return an output bundle path. The
example registers `repl_tools(overflow = "files")` plus two ordinary `ellmer`
tools:

- `tool_list_dir()`: provides `list_dir(path)` to list files in the bundle.
- `tool_read_file()`: provides `read_file(path, start_line, max_lines)` to read a line-numbered window
  from `transcript.txt` or `events.log`.

Run it with:

```sh
Rscript examples/ellmer-mcp-repl-files.R
```

Ordinary workspace files can still be read from R with `list.files()`,
`readLines()`, or `read.csv()` through the `repl` tool.
