# Use mcp-repl as an R REPL tool from ellmer.
#
# Run from the repository root:
# Rscript examples/ellmer-mcp-repl.R

library(ellmer)
source(file.path("examples", "ellmer-mcp-repl-helpers.R"))

stopifnot(nzchar(Sys.getenv("OPENAI_API_KEY")))

tools <- mcp_repl_tools("pager")
repl <- tool_by_name(tools, "repl")
print(repl(input = "cat('mcp-repl ready\\n')\n", timeout_ms = 10000))

chat <- chat_openai(
  system_prompt = paste(
    "You can use the repl tool to run R code in a persistent mcp-repl session.",
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
