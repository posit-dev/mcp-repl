# Shared helpers for the ellmer examples.
#
# install.packages(c("ellmer", "mcptools", "jsonlite", "glue"))
# Sys.setenv(OPENAI_API_KEY = "...")

mcp_repl_tools <- function(oversized_output = c("files", "pager")) {
  oversized_output <- match.arg(oversized_output)

  mcp_repl <- Sys.getenv("MCP_REPL_BINARY")
  if (!nzchar(mcp_repl)) {
    mcp_repl <- Sys.which("mcp-repl")
  }
  stopifnot(nzchar(mcp_repl), nzchar(Sys.which(mcp_repl)))

  # mcptools::mcp_tools() currently takes a config file path, so write a small
  # temporary config and load tools from that.
  config_file <- tempfile("mcp-repl-", fileext = ".json")
  jsonlite::write_json(
    list(
      "mcpServers" = list(
        "mcp-repl-r" = list(
          "command" = unname(mcp_repl),
          "args" = list(
            "--interpreter", "r",
            "--sandbox", "workspace-write",
            "--oversized-output", oversized_output
          )
        )
      )
    ),
    config_file,
    auto_unbox = TRUE,
    pretty = TRUE
  )

  tools <- mcptools::mcp_tools(config = config_file)
  tool_names <- vapply(tools, function(tool) tool@name, character(1))
  stopifnot("repl" %in% tool_names, "repl_reset" %in% tool_names)
  tools
}

tool_by_name <- function(tools, name) {
  tool_names <- vapply(tools, function(tool) tool@name, character(1))
  tools[[match(name, tool_names)]]
}

list_directory <- function(path) {
  stopifnot(is.character(path), length(path) == 1L, dir.exists(path))

  entries <- list.files(path, all.files = TRUE, no.. = TRUE, full.names = TRUE)
  if (length(entries) == 0L) {
    return("type       size path\n[empty directory]")
  }

  info <- file.info(entries)
  type <- ifelse(dir.exists(entries), "dir", "file")
  size <- ifelse(is.na(info$size), "", info$size)
  rows <- sprintf("%-4s %10s %s", type, size, basename(entries))
  paste(c("type       size path", rows), collapse = "\n")
}

read_text_file <- function(path, start_line = 1L, max_lines = 100L) {
  stopifnot(is.character(path), length(path) == 1L, file.exists(path))
  stopifnot(is.numeric(start_line), start_line >= 1)
  stopifnot(is.numeric(max_lines), max_lines > 0)

  lines <- readLines(path, warn = FALSE)
  if (start_line > length(lines)) {
    return("[end of file]")
  }

  end_line <- min(length(lines), start_line + max_lines - 1L)
  line_numbers <- seq.int(start_line, end_line)
  text <- paste(sprintf("%d | %s", line_numbers, lines[line_numbers]), collapse = "\n")

  if (end_line < length(lines)) {
    text <- glue::glue("{text}\n\n[truncated: next start_line = {end_line + 1L}]")
  }
  as.character(text)
}

bundle_tools <- function() {
  list(
    ellmer::tool(
      list_directory,
      name = "list_directory",
      description = "List the files in an mcp-repl output bundle directory.",
      arguments = list(
        path = ellmer::type_string("Path to the output bundle directory.")
      )
    ),
    ellmer::tool(
      read_text_file,
      name = "read_text_file",
      description = paste(
        "Read a text file by line number.",
        "Use this for transcript.txt or events.log from an mcp-repl output bundle."
      ),
      arguments = list(
        path = ellmer::type_string("Path to transcript.txt or events.log."),
        start_line = ellmer::type_integer(
          "First 1-based line number to read.",
          required = FALSE
        ),
        max_lines = ellmer::type_integer(
          "Maximum number of lines to read.",
          required = FALSE
        )
      )
    )
  )
}
