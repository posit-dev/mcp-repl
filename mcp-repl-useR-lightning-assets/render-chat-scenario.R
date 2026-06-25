suppressWarnings(suppressPackageStartupMessages({
  library(bslib)
  library(ellmer)
  library(htmltools)
  library(shiny)
  library(shinychat)
  library(webshot2)
}))

file_arg <- grep("^--file=", commandArgs(FALSE), value = TRUE)
stopifnot(length(file_arg) == 1)
asset_dir <- dirname(normalizePath(sub("^--file=", "", file_arg)))

bash_call <- function(id, command) {
  stopifnot(length(id) == 1, length(command) == 1)

  request <- ContentToolRequest(
    id = id,
    name = "bash",
    arguments = list(command = command)
  )
  card <- contents_shinychat(
    ContentToolResult(
      value = "",
      request = request
    )
  )
  card$intent <- command
  card$expanded <- TRUE
  card$show_request <- TRUE
  as.tags(card)
}

render_chat <- function(filename, assistant_text, commands) {
  stopifnot(length(filename) == 1, length(assistant_text) == 1, length(commands) >= 1)

  messages <- list(
    list(
      role = "user",
      content = "Can you explore this dataset and build a first model?"
    ),
    list(
      role = "assistant",
      content = do.call(
        tagList,
        c(
          list(tags$p(assistant_text)),
          mapply(
            function(id, command) bash_call(id, command),
            paste0("call_", seq_along(commands)),
            commands,
            SIMPLIFY = FALSE
          )
        )
      )
    )
  )

  ui <- page_fillable(
    theme = bs_theme(
      bg = "#141821",
      fg = "#f5f7fb",
      primary = "#315cf6"
    ),
    tags$style(HTML("
      body {
        margin: 0;
        background: #141821;
        font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
      }
      .shot-stage {
        min-height: 100vh;
        display: grid;
        place-items: center;
        padding: 24px;
      }
      .chat-card {
        width: 850px;
        height: 610px;
        background: #f7f8fb;
        border: 1px solid #dce2ec;
        border-radius: 8px;
        padding: 24px;
        box-shadow: 0 30px 90px rgba(0, 0, 0, .30);
      }
      .chat-card shiny-chat-container {
        --bs-body-bg: #f7f8fb;
        --bs-body-color: #172033;
        --bs-body-color-rgb: 23, 32, 51;
        --bs-secondary-color: #626b7a;
        --bs-border-color: #dce2ec;
        --bs-primary: #315cf6;
        --bs-primary-rgb: 49, 92, 246;
        --bs-code-color: #173a9d;
        --shiny-chat-user-message-bg: #315cf6;
        height: 560px;
        color: #172033;
      }
      .chat-card .shiny-chat-user-message {
        color: #ffffff;
      }
      .chat-card .shiny-chat-message:not(.shiny-chat-user-message) {
        color: #172033;
      }
      .chat-card code {
        color: #173a9d;
        background: #edf1ff;
        border-radius: 5px;
        padding: 2px 6px;
      }
      .chat-card shiny-tool-request,
      .chat-card shiny-tool-result {
        margin: .5rem 0;
        font-size: .88rem;
      }
      .chat-card .tool-intent {
        color: #334155;
        font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
        font-size: .82rem;
        font-style: normal;
        opacity: .9;
        white-space: pre-line;
      }
    ")),
    div(
      class = "shot-stage",
      div(
        class = "chat-card",
        chat_ui(
          "chat",
          messages = messages,
          placeholder = "Ask about the R project...",
          width = "100%",
          height = "560px",
          fill = FALSE
        )
      )
    )
  )

  server <- function(input, output, session) {}

  appshot(
    shinyApp(ui, server),
    file = file.path(asset_dir, filename),
    vwidth = 960,
    vheight = 700,
    selector = ".chat-card",
    expand = 4,
    delay = 1
  )
}

inline_command <- paste(
  "Rscript -e \"library(readr); library(dplyr);",
  "df <- read_csv('sales.csv');",
  "glimpse(df);",
  "summary(df$revenue)\""
)

heredoc_command <- paste(
  c(
    "cat <<'EOF' > /tmp/analysis.R",
    "library(readr)",
    "library(dplyr)",
    "df <- read_csv('sales.csv')",
    "df <- filter(df, !is.na(revenue))",
    "print(glimpse(df))",
    "EOF",
    "Rscript /tmp/analysis.R"
  ),
  collapse = "\n"
)

render_chat(
  "chat-rscript-e.png",
  "I'll inspect the data first, so I'll send R a quick one-off command.",
  inline_command
)

render_chat(
  "chat-heredoc-script.png",
  "That inline command is getting long, so I'll write a temporary script.",
  heredoc_command
)

render_chat(
  "chat-rerun-loop.png",
  "Now I'll iterate: change the file, run it, inspect the result, repeat.",
  c(
    "Rscript /tmp/analysis.R",
    "Rscript /tmp/analysis.R",
    "Rscript /tmp/analysis.R"
  )
)

render_chat(
  "chat-scenario.png",
  "I'll inspect the data, clean it up, and make a plot.",
  c(
    "Rscript -e \"...\"",
    "Rscript /tmp/analysis.R",
    "Rscript /tmp/analysis.R"
  )
)
