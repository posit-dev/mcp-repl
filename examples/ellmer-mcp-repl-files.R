# Use mcp-repl files-mode output bundles from ellmer.
#
# Run from the repository root:
# Rscript examples/ellmer-mcp-repl-files.R

library(ellmer)
source(file.path("examples", "ellmer-mcp-repl-helpers.R"))

stopifnot(nzchar(Sys.getenv("OPENAI_API_KEY")))

tools <- mcp_repl_tools("files")
repl <- tool_by_name(tools, "repl")
print(repl(input = "cat('mcp-repl ready\\n')\n", timeout_ms = 10000))

chat <- chat_openai(
  system_prompt = paste(
    "You can use the repl tool to run R code in a persistent mcp-repl session.",
    "When repl returns an output bundle path, use list_directory first.",
    "Then use read_text_file for transcript.txt or events.log when present.",
    "read_text_file returns line-numbered text and a next start_line hint.",
    "Use R functions such as list.files(), readLines(), or read.csv() for workspace files.",
    "Keep final answers concise."
  ),
  echo = "output"
)
chat$set_tools(c(tools, bundle_tools()))

answer <- chat$chat(
  "Use the R REPL to print 2000 numbered lines, then inspect transcript.txt."
)
cat(answer, "\n")
