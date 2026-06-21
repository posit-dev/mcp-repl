# Use mcp-repl as an R REPL tool from ellmer.
#
# Prerequisites:
# install.packages(c("ellmer", "mcptools", "jsonlite"))
# Sys.setenv(OPENAI_API_KEY = "...")
#
# If mcp-repl is not on PATH, set MCP_REPL_BINARY to the command or full path:
# Sys.setenv(MCP_REPL_BINARY = "/Users/alice/.cargo/bin/mcp-repl")

library(ellmer)
library(mcptools)
library(jsonlite)

mcp_repl <- Sys.getenv("MCP_REPL_BINARY")
if (!nzchar(mcp_repl)) {
  mcp_repl <- Sys.which("mcp-repl")
}

stopifnot(nzchar(mcp_repl))
stopifnot(nzchar(Sys.which(mcp_repl)))
stopifnot(nzchar(Sys.getenv("OPENAI_API_KEY")))

config_file <- tempfile("mcp-repl-", fileext = ".json")

# mcptools::mcp_tools() currently takes a config file path, not an inline
# payload, so this example materializes a temporary Claude-style JSON config.
config <- list(
  "mcpServers" = list(
    "mcp-repl-r" = list(
      "command" = unname(mcp_repl),
      "args" = list(
        "--interpreter", "r",
        "--sandbox", "workspace-write",
        "--oversized-output", "pager"
      )
    )
  )
)

jsonlite::write_json(config, config_file, auto_unbox = TRUE, pretty = TRUE)

tools <- mcptools::mcp_tools(config = config_file)
tool_names <- vapply(tools, function(tool) tool@name, character(1))

stopifnot("repl" %in% tool_names)
stopifnot("repl_reset" %in% tool_names)

repl <- tools[[match("repl", tool_names)]]
print(repl(input = "cat('mcp-repl ready\\n')\n", timeout_ms = 10000))

chat <- ellmer::chat_openai(
  system_prompt = paste(
    "You can use the repl tool to run R code in a persistent mcp-repl session.",
    "Run code when it helps you inspect data or verify a calculation.",
    "Use R functions such as readLines() or read.csv() to read workspace files.",
    "Keep final answers concise."
  ),
  echo = "output"
)
chat$set_tools(tools)

answer <- chat$chat(
  "Use the R REPL to compute the average mpg in the built-in mtcars data."
)
cat(answer, "\n")
