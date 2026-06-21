# Examples

This folder contains small starter files for using `mcp-repl` from `ellmer`.

The runnable scripts are the files to copy first. Shared setup lives in
[`examples/ellmer-mcp-repl-helpers.R`](ellmer-mcp-repl-helpers.R) so the scripts
stay focused on the shape of the `ellmer` workflow.

Prerequisites:

- Install `mcp-repl` and make sure `mcp-repl` is on `PATH`, or set
  `MCP_REPL_BINARY` to the command or full path.
- Install the R packages:

```r
install.packages(c("ellmer", "mcptools", "jsonlite", "glue"))
```

- Set credentials for the provider used by `ellmer::chat_openai`:

```r
Sys.setenv(OPENAI_API_KEY = "...")
```

## Pager Overflow

[`examples/ellmer-mcp-repl.R`](ellmer-mcp-repl.R) starts `mcp-repl` with
`--oversized-output pager` and registers the MCP `repl` tools on an `ellmer`
chat.

Run it with:

```sh
Rscript examples/ellmer-mcp-repl.R
```

## Files Overflow

[`examples/ellmer-mcp-repl-files.R`](ellmer-mcp-repl-files.R) uses
`--oversized-output files`. Large replies may return an output bundle path. The
example adds two ordinary `ellmer` tools:

- `list_directory(path)`: list files in the bundle.
- `read_text_file(path, start_line, max_lines)`: read a line-numbered window
  from `transcript.txt` or `events.log`.

Run it with:

```sh
Rscript examples/ellmer-mcp-repl-files.R
```

Ordinary workspace files can still be read from R with `list.files()`,
`readLines()`, or `read.csv()` through the `repl` tool.
