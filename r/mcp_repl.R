# mcp-repl embedded R code
#
# This file is evaluated after the embedded R session finishes its normal
# initialization. It is also used by a separate `R --vanilla` invocation to
# discover R home directories.

options(help_type = "text", pdfviewer = "false", width = 120, max.print = 200)
tools::Rd2txt_options(underline_titles = FALSE)
Sys.setenv(RETICULATE_PYTHON = "managed")
if (nzchar(Sys.getenv("CODEX_SANDBOX_NETWORK_DISABLED"))) {
  Sys.setenv(UV_OFFLINE = "1")
}

.mcp_repl_is_print_env_mode <- function() {
  args <- commandArgs(trailingOnly = TRUE)
  length(args) >= 1L && identical(args[[1L]], "--mcp-repl-print-env")
}

local({
  .mcp_repl_cat_file <- function(path) {
    if (!is.character(path) || length(path) != 1L || !nzchar(path)) {
      return(invisible(NULL))
    }
    if (!file.exists(path)) {
      return(invisible(NULL))
    }

    con <- file(path, open = "rb")
    on.exit(close(con), add = TRUE)

    repeat {
      chunk <- readChar(con, nchars = 65536L, useBytes = TRUE)
      if (!is.character(chunk) || length(chunk) != 1L || !nzchar(chunk)) {
        break
      }
      cat(chunk, sep = "")
    }

    invisible(NULL)
  }

  # Redirect R's pager integration (e.g. file.show(), help()) into stdout so
  # mcp-repl can capture and page it with its built-in pager mode.
  .mcp_repl_pager <- function(files,
                                header = NULL,
                                title = NULL,
                                delete.file = FALSE,
                                ...) {
    files <- as.character(files)
    if (length(files) == 0L) {
      return(invisible(NULL))
    }

    if (!is.null(title) && length(title) >= 1L && nzchar(title[[1L]])) {
      cat(title[[1L]], "\n", sep = "")
    }

    header_for <- function(i) {
      if (is.null(header) || length(header) == 0L) {
        return("")
      }
      header <- as.character(header)
      if (length(header) == 1L) {
        return(header[[1L]])
      }
      if (length(header) >= i) {
        return(header[[i]])
      }
      ""
    }

    for (i in seq_along(files)) {
      path <- files[[i]]
      if (!nzchar(path) || !file.exists(path)) {
        next
      }

      hdr <- header_for(i)
      if (nzchar(hdr)) {
        cat(hdr, "\n", sep = "")
      }

      .mcp_repl_cat_file(path)
      if (isTRUE(delete.file)) {
        unlink(path, force = TRUE)
      }
    }

    invisible(NULL)
  }

  options(pager = .mcp_repl_pager)
  options(help.pager = .mcp_repl_pager)
})

local({
  .mcp_repl_extract_md_fragment <- function(md, fragment) {
    if (!is.character(md) || length(md) != 1L || !nzchar(md)) {
      return(NULL)
    }
    if (
      !is.character(fragment) || length(fragment) != 1L || !nzchar(fragment)
    ) {
      return(NULL)
    }

    lines <- strsplit(md, "\n", fixed = TRUE)[[1L]]
    if (length(lines) == 0L) {
      return(NULL)
    }

    pattern <- paste0("(#", fragment)
    is_heading <- function(x) grepl("^#+\\s", sub("^\\s+", "", x))
    has_anchor <- function(x) grepl(pattern, x, fixed = TRUE)

    hits_heading <- which(is_heading(lines) & has_anchor(lines))
    hits_any <- which(has_anchor(lines))
    idx <- if (length(hits_heading)) {
      hits_heading[[1L]]
    } else if (length(hits_any)) {
      hits_any[[1L]]
    } else {
      NA_integer_
    }
    if (is.na(idx) || idx < 1L) {
      return(NULL)
    }

    if (is_heading(lines[[idx]])) {
      head_line <- sub("^\\s+", "", lines[[idx]])
      m <- regexpr("^#+", head_line)
      level <- attr(m, "match.length")
      if (!is.finite(level) || level < 1L) {
        level <- 1L
      }

      end <- length(lines) + 1L
      if (idx < length(lines)) {
        for (j in seq.int(idx + 1L, length(lines))) {
          line_j <- sub("^\\s+", "", lines[[j]])
          if (!is_heading(line_j)) {
            next
          }
          m2 <- regexpr("^#+", line_j)
          level2 <- attr(m2, "match.length")
          if (is.finite(level2) && level2 <= level) {
            end <- j
            break
          }
        }
      }
      return(paste(lines[seq.int(idx, end - 1L)], collapse = "\n"))
    }

    start <- max(1L, idx - 30L)
    end <- min(length(lines), idx + 60L)
    paste(lines[seq.int(start, end)], collapse = "\n")
  }

  .mcp_repl_extract_html_fragment <- function(lines, fragment) {
    if (!is.character(lines) || length(lines) == 0L) {
      return(NULL)
    }
    if (
      !is.character(fragment) || length(fragment) != 1L || !nzchar(fragment)
    ) {
      return(NULL)
    }

    pattern1 <- paste0("id=\"", fragment, "\"")
    pattern2 <- paste0("id='", fragment, "'")
    idx <- which(
      grepl(pattern1, lines, fixed = TRUE) |
        grepl(pattern2, lines, fixed = TRUE)
    )
    if (length(idx) == 0L) {
      return(NULL)
    }

    start <- idx[[1L]]
    end <- length(lines)
    if (start < length(lines)) {
      for (j in seq.int(start + 1L, length(lines))) {
        line_j <- lines[[j]]
        if (
          grepl("^\\s*<div class=\"(section|subsection)-level-extent\"", line_j)
        ) {
          end <- j - 1L
          break
        }
      }
    }

    paste(lines[seq.int(start, end)], collapse = "\n")
  }

  options(browser = function(url, ...) {
    url <- as.character(url)[1L]
    if (!nzchar(url)) {
      return(invisible(0))
    }

    path <- url
    fragment <- ""
    if (grepl("#", path, fixed = TRUE)) {
      parts <- strsplit(path, "#", fixed = TRUE)[[1L]]
      path <- parts[[1L]]
      if (length(parts) > 1L) fragment <- paste(parts[-1L], collapse = "#")
    }

    if (grepl("^file:", path)) {
      path <- sub("^file:", "", path)
    }
    if (grepl("^//", path)) {
      path <- sub("^//", "", path)
    }
    path <- utils::URLdecode(path)
    if (.Platform$OS.type == "windows" && grepl("^/[A-Za-z]:", path)) {
      path <- sub("^/", "", path)
    }

    if (file.exists(path)) {
      shown <- if (nzchar(fragment)) paste0(path, "#", fragment) else path
      cat(sprintf("[repl] browseURL file: %s\n\n", shown))

      if (grepl("\\.html?$", path, ignore.case = TRUE)) {
        if (nzchar(fragment)) {
          lines <- readLines(path, warn = FALSE)
          section <- .mcp_repl_extract_html_fragment(lines, fragment)
          html <- if (
            is.character(section) && length(section) == 1L && nzchar(section)
          ) {
            section
          } else {
            paste(lines, collapse = "\n")
          }
          md <- tryCatch(
            .Call("mcp_repl_htmd_html_to_markdown", html),
            error = function(e) NULL
          )
        } else {
          md <- tryCatch(
            .Call("mcp_repl_htmd_file_to_markdown", path),
            error = function(e) NULL
          )
        }

        if (is.character(md) && length(md) == 1L && nzchar(md)) {
          cat(md, "\n", sep = "")
          return(invisible(0))
        }
      }

      lines <- readLines(path, warn = FALSE)
      if (nzchar(fragment)) {
        section <- .mcp_repl_extract_html_fragment(lines, fragment)
        if (is.character(section) && length(section) == 1L && nzchar(section)) {
          cat(section, "\n", sep = "")
          return(invisible(0))
        }
      }

      cat(lines, sep = "\n")
      cat("\n")
      return(invisible(0))
    }

    cat(sprintf("[repl] browseURL blocked: %s\n", url))
    invisible(0)
  })
})

local({
  if (.mcp_repl_is_print_env_mode()) {
    return(invisible(NULL))
  }

  .mcp_repl_error_state <- new.env(parent = emptyenv())
  .mcp_repl_error_state$installed <- FALSE

  .mcp_repl_install_error_handler <- function() {
    if (isTRUE(.mcp_repl_error_state$installed)) {
      return(invisible(NULL))
    }

    .mcp_repl_error_state$installed <- TRUE
    previous <- getOption("error")
    options(mcp_repl.previous_error = previous)

    options(error = function() {
      try(.Call("mcp_repl_clear_pending_input"), silent = TRUE)
      handler <- getOption("mcp_repl.previous_error")
      if (is.function(handler)) {
        handler()
      } else if (!isTRUE(getOption("show.error.messages", TRUE))) {
        msg <- geterrmessage()
        if (is.character(msg) && length(msg) >= 1L && nzchar(msg[[1L]])) {
          msg <- msg[[1L]]
          if (!endsWith(msg, "\n")) {
            msg <- paste0(msg, "\n")
          }
          cat(msg, file = stderr())
        }
      }
      NULL
    })

    invisible(NULL)
  }

  .mcp_repl_install_error_handler()
})

local({
  .mcp_repl_normalize_rshowdoc_type <- function(type) {
    type <- tolower(as.character(type)[1L])
    if (!nzchar(type)) {
      return("html")
    }
    if (type == "text") {
      return("txt")
    }
    if (type == "htm") {
      return("html")
    }
    type
  }

  .mcp_repl_has_manual <- function(what, ext) {
    if (!is.character(what) || length(what) < 1L) {
      return(FALSE)
    }
    what <- what[[1L]]
    if (!nzchar(what)) {
      return(FALSE)
    }
    path <- file.path(R.home("doc"), "manual", paste0(what, ".", ext))
    file.exists(path)
  }

  assign(
    "RShowDoc",
    function(what, type, package) {
      if (missing(type)) {
        type <- "html"
      } else {
        type <- .mcp_repl_normalize_rshowdoc_type(type)
      }

      has_package <- FALSE
      if (!missing(package)) {
        pkg <- as.character(package)[1L]
        if (!is.na(pkg) && nzchar(pkg)) {
          has_package <- TRUE
        }
      }

      if (!has_package && identical(type, "txt")) {
        if (
          !.mcp_repl_has_manual(what, "txt") &&
            (.mcp_repl_has_manual(what, "html") ||
              .mcp_repl_has_manual(what, "htm"))
        ) {
          type <- "html"
        }
      }

      err <- tryCatch(
        {
          utils::RShowDoc(what = what, type = type, package = package)
          NULL
        },
        error = function(e) e
      )
      if (inherits(err, "error") && identical(type, "txt") && !has_package) {
        err2 <- tryCatch(
          {
            utils::RShowDoc(what = what, type = "html", package = package)
            NULL
          },
          error = function(e) e
        )
        if (!inherits(err2, "error")) {
          return(invisible(NULL))
        }
      }
      if (inherits(err, "error")) {
        stop(err)
      }
      invisible(NULL)
    },
    envir = .GlobalEnv
  )
})

local({
  vignette_method <- function(x, ...) {
    topic <- if (is.null(x$Topic)) "" else as.character(x$Topic)
    package <- if (is.null(x$Package)) "" else as.character(x$Package)
    title <- if (is.null(x$Title)) "" else as.character(x$Title)

    cat(sprintf("[repl] vignette: %s (package: %s)\n", topic, package))
    if (nzchar(title)) {
      cat(sprintf("Title: %s\n", title))
    }

    if (is.null(x$Dir) || !nzchar(x$Dir)) {
      cat("[repl] vignette directory not found\n")
      return(invisible(x))
    }

    path_src <- if (!is.null(x$File) && nzchar(x$File)) {
      file.path(x$Dir, "doc", x$File)
    } else {
      NA_character_
    }
    path_r <- if (!is.null(x$R) && nzchar(x$R)) {
      file.path(x$Dir, "doc", x$R)
    } else {
      NA_character_
    }
    path_rendered <- if (!is.null(x$PDF) && nzchar(x$PDF)) {
      file.path(x$Dir, "doc", x$PDF)
    } else {
      NA_character_
    }

    if (is.character(path_src) && nzchar(path_src) && file.exists(path_src)) {
      cat(sprintf("Source: %s\n\n", path_src))
      cat(readLines(path_src, warn = FALSE), sep = "\n")
      cat("\n")
      return(invisible(x))
    }

    if (is.character(path_r) && nzchar(path_r) && file.exists(path_r)) {
      cat(sprintf("R code: %s\n\n", path_r))
      cat(readLines(path_r, warn = FALSE), sep = "\n")
      cat("\n")
      return(invisible(x))
    }

    if (
      is.character(path_rendered) &&
        nzchar(path_rendered) &&
        file.exists(path_rendered)
    ) {
      ext <- tolower(tools::file_ext(path_rendered))
      if (ext %in% c("html", "htm")) {
        cat(sprintf("Rendered: %s\n\n", path_rendered))
        browseURL(path_rendered)
        return(invisible(x))
      }
      if (ext %in% c("txt", "md")) {
        cat(sprintf("Rendered: %s\n\n", path_rendered))
        cat(readLines(path_rendered, warn = FALSE), sep = "\n")
        cat("\n")
        return(invisible(x))
      }
      if (ext == "pdf") {
        cat(sprintf("Rendered: %s\n", path_rendered))
        cat("[repl] PDF vignettes can't be displayed as text yet.\n")
        return(invisible(x))
      }
    }

    cat("[repl] no readable vignette file found\n")
    invisible(x)
  }

  browse_vignettes_method <- function(x, ...) {
    if (length(x) == 0L) {
      cat("[repl] no vignettes found\n")
      return(invisible(x))
    }

    call <- attr(x, "call")
    if (!is.null(call)) {
      cat(sprintf("[repl] browseVignettes: %s\n\n", deparse(call)))
    }

    for (pkg in names(x)) {
      cat(sprintf("Package: %s\n", pkg))
      info <- x[[pkg]]
      if (is.null(dim(info)) || nrow(info) == 0L) {
        cat("  (none)\n\n")
        next
      }
      for (i in seq_len(nrow(info))) {
        topic <- info[i, "Topic"]
        title <- info[i, "Title"]
        cat(sprintf("  - %s: %s\n", topic, title))
      }
      cat("\n")
    }

    cat("Tip: use vignette(<topic>, package=<pkg>) to get file paths.\n")
    invisible(x)
  }

  registerS3method("print", "vignette", vignette_method)
  registerS3method("print", "browseVignettes", browse_vignettes_method)
})

local({
  if (.mcp_repl_is_print_env_mode()) {
    return(invisible(NULL))
  }

  .mcp_repl_plot_state <- new.env(parent = emptyenv())
  .mcp_repl_plot_state$counter <- 0L
  .mcp_repl_plot_state$current_id <- NULL
  .mcp_repl_plot_state$next_is_new <- TRUE
  .mcp_repl_plot_state$page_open_seen <- FALSE
  .mcp_repl_plot_state$recordings <- new.env(parent = emptyenv())
  .mcp_repl_plot_state$in_render <- FALSE
  .mcp_repl_plot_state$initialized <- FALSE
  .mcp_repl_plot_default_dpi <- 96
  .mcp_repl_plot_default_width <- 800 / .mcp_repl_plot_default_dpi
  .mcp_repl_plot_default_height <- 600 / .mcp_repl_plot_default_dpi
  .mcp_repl_plot_default_units <- "in"

  .mcp_repl_plot_units <- function(units) {
    if (!is.character(units) || length(units) != 1L || !nzchar(units)) {
      return(NULL)
    }

    units <- tolower(units)
    if (units %in% c("in", "inch", "inches")) {
      return("in")
    }
    if (units %in% c("cm", "centimeter", "centimeters")) {
      return("cm")
    }
    if (units %in% c("mm", "millimeter", "millimeters")) {
      return("mm")
    }
    if (units %in% c("px", "pixel", "pixels")) {
      return("px")
    }

    NULL
  }

  .mcp_repl_plot_device <- function(...) {
    path <- tempfile("mcp-repl-plot-", fileext = ".png")
    ok <- FALSE
    tryCatch({
      grDevices::png(filename = path, ...)
      ok <- TRUE
    }, error = function(e) NULL)

    if (!ok) {
      path <- tempfile("mcp-repl-plot-", fileext = ".pdf")
      grDevices::pdf(file = path, ...)
    }

    try(grDevices::dev.control(displaylist = "enable"), silent = TRUE)
    invisible(NULL)
  }

  options(device = .mcp_repl_plot_device)

  .mcp_repl_new_plot_id <- function() {
    st <- .mcp_repl_plot_state
    st$counter <- st$counter + 1L
    sprintf("plot-%d-%d", Sys.getpid(), st$counter)
  }

  .mcp_repl_plot_render_recording <- function(recording, width, height, res) {
    st <- .mcp_repl_plot_state
    if (isTRUE(st$in_render)) {
      return(NULL)
    }

    st$in_render <- TRUE
    on.exit({
      st$in_render <- FALSE
    }, add = TRUE)

    path <- tempfile("mcp-repl-plot-", fileext = ".png")
    old_dev <- grDevices::dev.cur()
    ok <- FALSE

    tryCatch({
      grDevices::png(
        filename = path,
        width = width,
        height = height,
        res = res
      )
      suppressWarnings(grDevices::replayPlot(recording))
      grDevices::dev.off()
      ok <- TRUE
    }, error = function(e) {
      try(grDevices::dev.off(), silent = TRUE)
    })

    if (!ok || !file.exists(path)) {
      return(NULL)
    }

    size <- file.info(path)$size
    if (!is.finite(size) || size <= 0) {
      unlink(path, force = TRUE)
      return(NULL)
    }

    data <- readBin(path, "raw", n = size)
    unlink(path, force = TRUE)

    if (is.numeric(old_dev) && old_dev > 1L) {
      try(grDevices::dev.set(old_dev), silent = TRUE)
    }

    data
  }

  .mcp_repl_plot_process_changes <- function(reason = "") {
    st <- .mcp_repl_plot_state
    if (isTRUE(st$in_render)) {
      return(invisible(NULL))
    }

    if (grDevices::dev.cur() <= 1L) {
      return(invisible(NULL))
    }

    recording <- tryCatch(grDevices::recordPlot(), error = function(e) NULL)
    if (is.null(recording)) {
      return(invisible(NULL))
    }

    raw_recording <- tryCatch(serialize(recording, NULL), error = function(e) NULL)
    if (is.null(raw_recording)) {
      return(invisible(NULL))
    }

    if (is.null(st$current_id)) {
      st$current_id <- .mcp_repl_new_plot_id()
      st$next_is_new <- TRUE
    }

    id <- st$current_id
    last_recording <- st$recordings[[id]]
    if (
      !isTRUE(st$next_is_new) &&
        is.raw(last_recording) &&
        identical(raw_recording, last_recording)
    ) {
      return(invisible(NULL))
    }

    st$recordings[[id]] <- raw_recording

    width <- getOption("console.plot.width")
    if (is.null(width)) {
      width <- .mcp_repl_plot_default_width
    }

    height <- getOption("console.plot.height")
    if (is.null(height)) {
      height <- .mcp_repl_plot_default_height
    }

    units <- .mcp_repl_plot_units(
      getOption("console.plot.units", .mcp_repl_plot_default_units)
    )
    if (is.null(units)) {
      units <- .mcp_repl_plot_default_units
    }

    dpi <- getOption("console.plot.dpi")
    if (is.null(dpi)) {
      dpi <- getOption("console.plot.res", .mcp_repl_plot_default_dpi)
    }

    if (!is.numeric(width) || !is.finite(width) || width <= 0) {
      width <- .mcp_repl_plot_default_width
    }
    if (!is.numeric(height) || !is.finite(height) || height <= 0) {
      height <- .mcp_repl_plot_default_height
    }
    if (!is.numeric(dpi) || !is.finite(dpi) || dpi <= 0) {
      dpi <- .mcp_repl_plot_default_dpi
    }

    scale <- switch(
      units,
      "in" = 1,
      "cm" = 1 / 2.54,
      "mm" = 1 / 25.4,
      "px" = NA_real_
    )
    if (is.na(scale)) {
      width <- round(width)
      height <- round(height)
    } else {
      width <- round(width * scale * dpi)
      height <- round(height * scale * dpi)
    }

    if (!is.finite(width) || width <= 0) {
      width <- round(.mcp_repl_plot_default_width * .mcp_repl_plot_default_dpi)
    }
    if (!is.finite(height) || height <= 0) {
      height <- round(.mcp_repl_plot_default_height * .mcp_repl_plot_default_dpi)
    }

    png_raw <- .mcp_repl_plot_render_recording(
      recording,
      width = width,
      height = height,
      res = dpi
    )
    if (!is.raw(png_raw)) {
      return(invisible(NULL))
    }

    is_new <- isTRUE(st$next_is_new)
    st$next_is_new <- FALSE
    try(
      .Call("mcp_repl_plot_emit", id, png_raw, "image/png", is_new),
      silent = TRUE
    )
    invisible(NULL)
  }

  .mcp_repl_plot_before_new_page <- function(reason = "") {
    st <- .mcp_repl_plot_state
    if (isTRUE(st$in_render)) {
      return(invisible(NULL))
    }

    is_grid <- identical(reason, "before.grid.newpage")
    is_new_page <- if (is_grid) {
      TRUE
    } else {
      isTRUE(par("page"))
    }
    has_recording <- !is.null(st$current_id) &&
      exists(st$current_id, envir = st$recordings, inherits = FALSE)
    if (has_recording || isTRUE(st$page_open_seen)) {
      .mcp_repl_plot_process_changes(reason)
    }

    if (is_new_page) {
      st$current_id <- .mcp_repl_new_plot_id()
      st$next_is_new <- TRUE
      st$page_open_seen <- TRUE
      st$recordings <- new.env(parent = emptyenv())
    } else {
      st$next_is_new <- FALSE
    }
    invisible(NULL)
  }

  .mcp_repl_plot_task_callback <- function(expr, value, ok, visible) {
    st <- .mcp_repl_plot_state
    if (!isTRUE(ok)) {
      try(.Call("mcp_repl_clear_pending_input"), silent = TRUE)
    }
    has_recording <- !is.null(st$current_id) &&
      exists(st$current_id, envir = st$recordings, inherits = FALSE)
    if (isTRUE(st$page_open_seen) || has_recording) {
      .mcp_repl_plot_process_changes("task_callback")
    }
    st$page_open_seen <- FALSE
    TRUE
  }

  .mcp_repl_plot_init <- function() {
    st <- .mcp_repl_plot_state
    if (isTRUE(st$initialized)) {
      return(invisible(NULL))
    }

    st$current_id <- .mcp_repl_new_plot_id()
    st$next_is_new <- TRUE
    st$initialized <- TRUE

    setHook("before.plot.new", action = "replace", function(...) {
      .mcp_repl_plot_before_new_page("before.plot.new")
    })
    setHook("before.grid.newpage", action = "replace", function(...) {
      .mcp_repl_plot_before_new_page("before.grid.newpage")
    })
    addTaskCallback(.mcp_repl_plot_task_callback, name = "mcp-repl-plots")

    invisible(NULL)
  }

  .mcp_repl_plot_init()
})

local({
  if (.mcp_repl_is_print_env_mode()) {
    cat(paste(R.home("share"), R.home("include"), R.home("doc"), sep = ";"))
  }
})
