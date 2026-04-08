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

  .mcp_repl_patch_reticulate_windows <- function() {
    if (
      .Platform$OS.type != "windows" ||
      !requireNamespace("reticulate", quietly = TRUE)
    ) {
      return(invisible(FALSE))
    }

    ns <- asNamespace("reticulate")
    original_py_help <- get("py_help", envir = ns)

    mcp_windows_python <- function() {
      python <- Sys.getenv("RETICULATE_PYTHON", unset = NA)
      if (!is.na(python) && !identical(python, "managed")) {
        return(normalizePath(python, winslash = "/", mustWork = FALSE))
      }
      python <- reticulate:::uv_get_or_create_env()
      normalizePath(python, winslash = "/", mustWork = FALSE)
    }

    mcp_windows_proxy <- function(python, module, name = NULL) {
      structure(
        list(python = python, module = module, name = name),
        class = c("mcp_repl_reticulate_proxy", "list")
      )
    }

    mcp_windows_run_python <- function(python, args) {
      system2(
        command = python,
        args = args,
        stdout = TRUE,
        stderr = TRUE,
        input = ""
      )
    }

    mcp_windows_render_help <- function(object) {
      python <- object$python
      module <- object$module
      name <- object$name
      if (is.null(name)) {
        name <- ""
      }
      script <- tempfile("mcp-reticulate-help-", fileext = ".py")
      on.exit(unlink(script), add = TRUE)
      writeLines(c(
        "import importlib",
        "import pydoc",
        "import sys",
        "obj = importlib.import_module(sys.argv[1])",
        "if len(sys.argv) > 2 and sys.argv[2]:",
        "    for part in sys.argv[2].split('.'):",
        "        obj = getattr(obj, part)",
        "sys.stdout.write(pydoc.render_doc(obj, renderer=pydoc.plaintext))"
      ), script)
      out <- mcp_windows_run_python(
        python,
        c(normalizePath(script, winslash = "/", mustWork = FALSE), module, name)
      )
      title <- paste(
        "Python Help:",
        if (nzchar(name)) name else module
      )
      tmp <- tempfile("py_help", fileext = ".txt")
      writeLines(out, con = tmp)
      file.show(tmp, title = title, delete.file = TRUE)
      invisible(NULL)
    }

    python_config_impl <- function(python) {
      if (!file.exists(python)) {
        msg <- paste0("Error running ", shQuote(python), ": No such file.")
        info <- reticulate:::python_info(python)
        if (info$type == "virtualenv") {
          msg <- paste0(
            sep = "",
            c(
              msg, "\n",
              "The Python installation used to create the virtualenv has been moved or removed",
              if (is.null(info$starter)) "." else ":\n  ",
              shQuote(info$starter)
            )
          )
        }
        stop(msg)
      }

      script <- system.file("config/config.py", package = "reticulate")
      config <- tryCatch(system2(
        command = python,
        args = shQuote(script),
        stdout = TRUE,
        stderr = FALSE,
        input = ""
      ), error = function(e) {
        e$message <- paste(e$message, shQuote(python))
        stop(e)
      })

      status <- attr(config, "status")
      if (!is.null(status)) {
        errmsg <- attr(config, "errmsg")
        stop("Error ", status, " occurred running ", python, ": ", errmsg)
      }

      if (reticulate:::is_osx()) {
        clt <- "/Library/Developer/CommandLineTools"
        xcode <- "/Applications/Xcode.app/Contents/Developer"
        if (file.exists(clt)) {
          config <- gsub(xcode, clt, config, fixed = TRUE)
        }
      }

      config
    }

    uv_get_or_create_env <- function(
      packages = reticulate:::py_reqs_get("packages"),
      python_version = reticulate:::py_reqs_get("python_version"),
      exclude_newer = reticulate:::py_reqs_get("exclude_newer")
    ) {
      uv <- reticulate:::uv_binary()
      if (is.null(uv)) {
        return()
      }

      withr::local_envvar(c(
        VIRTUAL_ENV = NA,
        if (reticulate:::is_positron()) c(RUST_LOG = NA),
        if (isTRUE(attr(uv, "reticulate-managed", TRUE))) {
          c(
            UV_CACHE_DIR = reticulate:::reticulate_cache_dir("uv", "cache"),
            UV_PYTHON_INSTALL_DIR = reticulate:::reticulate_cache_dir("uv", "python")
          )
        }
      ))

      resolved_python_version <- reticulate:::resolve_python_version(
        constraints = python_version,
        uv = uv
      )
      if (!length(resolved_python_version)) {
        return()
      }

      call_args <- list(
        packages = packages,
        python_version = if (is.null(python_version)) {
          paste(resolved_python_version, "(reticulate default)")
        } else {
          python_version
        },
        exclude_newer = exclude_newer
      )

      if (length(packages)) {
        packages <- as.vector(rbind("--with", packages))
      }

      python_version <- c("--python", resolved_python_version)

      if (!is.null(exclude_newer)) {
        exclude_newer <- c("--exclude-newer", exclude_newer)
      }

      uv_output_file <- tempfile()
      on.exit(unlink(uv_output_file), add = TRUE)

      uv_args <- c(
        "tool", "run",
        "--isolated",
        python_version,
        exclude_newer,
        packages,
        "--",
        "python", "-c",
        "import sys; f=open(sys.argv[-1], chr(119)); f.write(sys.executable); f.close();",
        uv_output_file
      )

      error_code <- suppressWarnings(system2(
        uv,
        reticulate:::maybe_shQuote(uv_args),
        input = ""
      ))

      if (error_code) {
        cat("uv error code: ", error_code, "\n", sep = "", file = stderr())
        msg <- do.call(reticulate:::py_reqs_format, call_args)
        writeLines(c(msg, strrep("-", 73L)), con = stderr())
        if (error_code == 2) {
          cat(
            "Hint: If you are temporarily offline, try setting `Sys.setenv(UV_OFFLINE=1)`.\n",
            file = stderr()
          )
        }

        if (any(call_args$packages %in% reticulate:::builtin_module_names)) {
          requested_builtin_modules <- intersect(
            call_args$packages,
            reticulate:::builtin_module_names
          )
          invalid <- unique(c("sys", "os", requested_builtin_modules))
          writeLines(con = stderr(), c(
            "Hint: `py_require()` expects Python package names rather than Python module names.",
            sprintf(
              "Modules provided by the Python standard library such as %s should not be passed to `py_require()`.",
              reticulate:::pc_and("`", invalid, "`")
            ),
            strrep("-", 73L)
          ))
        }

        stop("Call `py_require()` to remove or replace conflicting requirements.")
      }

      cached_python <- readLines(uv_output_file, warn = FALSE)
      cached_python
    }

    py_config <- function() {
      python <- mcp_windows_python()
      config <- reticulate:::python_config(python)
      config$available <- TRUE
      config
    }

    import_builtins <- function(convert = TRUE, delay_load = FALSE) {
      python <- mcp_windows_python()
      module <- mcp_windows_proxy(python, "builtins")
      class(module) <- c("mcp_repl_reticulate_module_proxy", class(module))
      module
    }

    py_help <- function(object) {
      if (inherits(object, "mcp_repl_reticulate_proxy")) {
        return(mcp_windows_render_help(object))
      }
      original_py_help(object)
    }

    `$.mcp_repl_reticulate_module_proxy` <- function(x, name) {
      data <- unclass(x)
      mcp_windows_proxy(data$python, data$module, name)
    }
    assign(
      "$.mcp_repl_reticulate_module_proxy",
      `$.mcp_repl_reticulate_module_proxy`,
      envir = .GlobalEnv
    )

    assignInNamespace("python_config_impl", python_config_impl, ns = "reticulate")
    assignInNamespace("uv_get_or_create_env", uv_get_or_create_env, ns = "reticulate")
    assignInNamespace("py_config", py_config, ns = "reticulate")
    assignInNamespace("import_builtins", import_builtins, ns = "reticulate")
    assignInNamespace("py_help", py_help, ns = "reticulate")
    invisible(TRUE)
  }

  setHook(
    packageEvent("reticulate", "onLoad"),
    function(...) .mcp_repl_patch_reticulate_windows(),
    action = "append"
  )
  if ("reticulate" %in% loadedNamespaces()) {
    .mcp_repl_patch_reticulate_windows()
  }
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

    .mcp_repl_plot_process_changes(reason)
    is_grid <- identical(reason, "before.grid.newpage")
    is_new_page <- if (is_grid) {
      TRUE
    } else {
      isTRUE(par("page"))
    }

    if (is_new_page) {
      st$current_id <- .mcp_repl_new_plot_id()
      st$next_is_new <- TRUE
      st$recordings <- new.env(parent = emptyenv())
    } else {
      st$next_is_new <- FALSE
    }
    invisible(NULL)
  }

  .mcp_repl_plot_task_callback <- function(expr, value, ok, visible) {
    if (!isTRUE(ok)) {
      try(.Call("mcp_repl_clear_pending_input"), silent = TRUE)
    }
    .mcp_repl_plot_process_changes("task_callback")
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
