# Use mcp-repl with files-mode overflow output from ellmer.
#
# Prerequisites:
# install.packages(c("ellmer", "mcptools", "jsonlite", "glue"))
# Sys.setenv(OPENAI_API_KEY = "...")
#
# If mcp-repl is not on PATH, set MCP_REPL_BINARY to the command or full path:
# Sys.setenv(MCP_REPL_BINARY = "/Users/alice/.cargo/bin/mcp-repl")

library(ellmer)
library(mcptools)
library(jsonlite)
library(glue)

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
        "--oversized-output", "files"
      )
    )
  )
)

jsonlite::write_json(config, config_file, auto_unbox = TRUE, pretty = TRUE)

tools <- mcptools::mcp_tools(config = config_file)
tool_names <- vapply(tools, function(tool) tool@name, character(1))

stopifnot("repl" %in% tool_names)
stopifnot("repl_reset" %in% tool_names)

list_directory <- function(path, recursive = FALSE, max_entries = 200L) {
  stopifnot(is.character(path), length(path) == 1L, nzchar(path))
  stopifnot(is.logical(recursive), length(recursive) == 1L, !is.na(recursive))
  stopifnot(is.numeric(max_entries), length(max_entries) == 1L, max_entries > 0)

  path <- path.expand(path)
  max_entries <- as.integer(max_entries)
  stopifnot(dir.exists(path), !is.na(max_entries))

  entries <- list.files(
    path,
    all.files = TRUE,
    no.. = TRUE,
    recursive = recursive,
    full.names = TRUE
  )
  entries <- head(entries, max_entries)
  header <- sprintf("%-4s %10s %s", "type", "size", "path")

  if (length(entries) == 0L) {
    return(paste(header, "[empty directory]", sep = "\n"))
  }

  info <- file.info(entries)
  type <- ifelse(dir.exists(entries), "dir", "file")
  size <- ifelse(is.na(info$size), "", as.character(info$size))
  root <- normalizePath(path, winslash = "/", mustWork = TRUE)
  normalized <- normalizePath(entries, winslash = "/", mustWork = FALSE)
  rel <- normalized
  in_root <- startsWith(normalized, paste0(root, "/"))
  rel[in_root] <- substring(normalized[in_root], nchar(root) + 2L)
  rows <- sprintf("%-4s %10s %s", type, size, rel)
  paste(c(header, rows), collapse = "\n")
}

list_directory_tool <- ellmer::tool(
  list_directory,
  name = "list_directory",
  description = paste(
    "List files in a local directory by path.",
    "Use this first for mcp-repl files-mode output bundle directories.",
    "The result has columns type, size, and path."
  ),
  arguments = list(
    path = ellmer::type_string(
      "Path to a directory, usually an mcp-repl output bundle directory."
    ),
    recursive = ellmer::type_boolean(
      "Whether to list nested files recursively.",
      required = FALSE
    ),
    max_entries = ellmer::type_integer(
      "Maximum number of directory entries to return.",
      required = FALSE
    )
  )
)

add_line_numbers <- function(text, first_line_number) {
  stopifnot(is.character(text), length(text) == 1L)
  stopifnot(is.numeric(first_line_number), length(first_line_number) == 1L)

  if (!nzchar(text)) {
    return(text)
  }

  lines <- strsplit(text, "\n", fixed = TRUE)[[1]]
  if (endsWith(text, "\n")) {
    lines <- c(lines, "")
  }

  numbers <- seq.int(first_line_number, length.out = length(lines))
  width <- nchar(as.character(tail(numbers, 1L)))
  paste(sprintf(paste0("%", width, "d | %s"), numbers, lines), collapse = "\n")
}

read_text_file <- function(path,
                           start_line = 1L,
                           max_lines = 200L,
                           max_bytes = 20000L) {
  stopifnot(is.character(path), length(path) == 1L, nzchar(path))
  stopifnot(
    is.numeric(start_line),
    length(start_line) == 1L,
    start_line >= 1
  )
  stopifnot(is.numeric(max_lines), length(max_lines) == 1L, max_lines > 0)
  stopifnot(is.numeric(max_bytes), length(max_bytes) == 1L, max_bytes > 0)

  path <- path.expand(path)
  start_line <- as.integer(start_line)
  max_lines <- as.integer(max_lines)
  max_bytes <- as.integer(max_bytes)
  stopifnot(
    file.exists(path),
    !dir.exists(path),
    !is.na(start_line),
    !is.na(max_lines),
    !is.na(max_bytes)
  )

  con <- file(path, open = "rb")
  on.exit(close(con), add = TRUE)

  line_number <- 1L
  while (line_number < start_line) {
    line <- readLines(con, n = 1L, warn = FALSE)
    if (length(line) == 0L) {
      return("[end of file]")
    }
    line_number <- line_number + 1L
  }

  output <- character()
  bytes_read <- 0L
  line_count <- 0L
  truncated <- FALSE

  while (line_count < max_lines) {
    text <- readLines(con, n = 1L, warn = FALSE)
    if (length(text) == 0L) {
      break
    }

    text <- iconv(text, from = "UTF-8", to = "UTF-8", sub = "byte")
    stopifnot(!is.na(text))

    formatted <- add_line_numbers(text, line_number)
    formatted_bytes <- nchar(formatted, type = "bytes") + 1L
    if (bytes_read + formatted_bytes > max_bytes) {
      if (length(output) == 0L) {
        return(as.character(glue(
          "[line {line_number} is larger than max_bytes = {max_bytes}; ",
          "increase max_bytes to read it]"
        )))
      }
      truncated <- TRUE
      break
    }

    output <- c(output, formatted)
    bytes_read <- bytes_read + formatted_bytes
    line_count <- line_count + 1L
    line_number <- line_number + 1L
  }

  next_start_line <- line_number
  if (line_count == max_lines) {
    truncated <- length(readLines(con, n = 1L, warn = FALSE)) > 0L
  }

  result <- paste(output, collapse = "\n")
  if (!nzchar(result) && !truncated) {
    return("[end of file]")
  }

  if (truncated) {
    as.character(
      glue(
        "{result}\n\n",
        "[truncated: next start_line = {next_start_line}]"
      )
    )
  } else {
    result
  }
}

read_text_file_tool <- ellmer::tool(
  read_text_file,
  name = "read_text_file",
  description = paste(
    "Read a local text file by path.",
    "Use this for mcp-repl files-mode output bundle files such as",
    "transcript.txt, or events.log when it is present.",
    "Reads at most max_lines lines and max_bytes bytes, decoding text as UTF-8.",
    "Pass the returned next start_line value to continue reading later lines.",
    "Returned text includes line numbers by default.",
    "Invalid bytes are shown with byte escapes."
  ),
  arguments = list(
    path = ellmer::type_string(
      "Path to a text file, usually transcript.txt or an existing events.log from an mcp-repl output bundle."
    ),
    start_line = ellmer::type_integer(
      "First 1-based line number to read. Use the returned next start_line value to continue.",
      required = FALSE
    ),
    max_lines = ellmer::type_integer(
      "Maximum number of lines to read.",
      required = FALSE
    ),
    max_bytes = ellmer::type_integer(
      "Maximum number of bytes to return.",
      required = FALSE
    )
  )
)

repl <- tools[[match("repl", tool_names)]]
print(repl(input = "cat('mcp-repl ready\\n')\n", timeout_ms = 10000))

chat <- ellmer::chat_openai(
  system_prompt = paste(
    "You can use the repl tool to run R code in a persistent mcp-repl session.",
    "When repl returns an output bundle path, use list_directory first.",
    "Then use read_text_file for text files inside that bundle, especially transcript.txt.",
    "read_text_file returns line-numbered text by default.",
    "To continue reading a file, pass read_text_file the returned next start_line value.",
    "Use events.log only when that file is present in the returned bundle.",
    "Use R functions such as list.files(), readLines(), or read.csv() for workspace files.",
    "Keep final answers concise."
  ),
  echo = "output"
)
chat$set_tools(c(tools, list(list_directory_tool, read_text_file_tool)))

answer <- chat$chat(
  "Use the R REPL to print 2000 lines, list the files-mode output bundle, then inspect transcript.txt."
)
cat(answer, "\n")
