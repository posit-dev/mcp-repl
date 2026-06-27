# Use mcp-repl files-mode output bundles from ellmer.
#
# Run from the repository root:
# Rscript examples/ellmer-mcp-repl-files.R

library(ellmer)
source(file.path("examples", "ellmer-mcp-repl-helpers.R"))

stopifnot(nzchar(Sys.getenv("OPENAI_API_KEY")))

r_home <- Sys.getenv("MCP_REPL_R_HOME")
if (!nzchar(r_home)) {
  r_home <- R.home()
}

chat <- chat_openai(
  system_prompt = paste(
    "Use the REPL tool to do analysis.",
    "Answer in one or two sentences."
  ),
  echo = "output"
)
chat$set_tools(c(
  repl_tools(overflow = "files", r_home = r_home),
  tool_read_file(),
  tool_list_dir()
))

answer <- chat$chat("Tell me something interesting about the penguins dataset.")
cat(answer, "\n")
