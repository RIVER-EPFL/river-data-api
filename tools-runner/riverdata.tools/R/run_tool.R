# The packages every tool script may use unqualified. Attached per request (OpenCPU forks a
# fresh session per call, so the search path never leaks between runs). The API's script lint
# whitelists library() calls to this same set plus Suggests.
SCRIPT_PACKAGES <- c("dplyr", "tidyr", "magrittr")

#' Execute a tool script against JSON-shaped inputs.
#'
#' `script` is the complete R source of one tool (the shared portal calculation functions plus a
#' wrapper); `entry` names the function inside it to call. The entry function receives the three
#' lists exactly as they arrived in the request and returns a named list, which OpenCPU
#' serialises back to JSON.
#'
#' The script is evaluated in a fresh environment. Curves and constants are resolved by the API
#' and arrive as values; nothing here reads a database or the network.
run_tool <- function(script, entry = "tool", inputs = list(), constants = list(), curves = list()) {
  with_structured_error({
    if (!is.character(script) || length(script) != 1L || !nzchar(script)) {
      stop("script must be a single non-empty string", call. = FALSE)
    }
    if (!is.character(entry) || length(entry) != 1L || !nzchar(entry)) {
      stop("entry must be a single non-empty string", call. = FALSE)
    }
    for (pkg in SCRIPT_PACKAGES) {
      suppressPackageStartupMessages(library(pkg, character.only = TRUE))
    }

    env <- new.env(parent = globalenv())
    eval(parse(text = script), envir = env)

    fn <- get0(entry, envir = env, mode = "function", inherits = FALSE)
    if (is.null(fn)) {
      stop(sprintf("entry function '%s' is not defined by the script", entry), call. = FALSE)
    }
    fn(inputs = inputs, constants = constants, curves = curves)
  })
}

#' Re-signal any error as a one-line JSON object, so the caller gets the traceback rather than
#' only the message.
#'
#' A script author reads these in the portal, and "object 'x' not found" without the frame it
#' happened in names no line to fix. OpenCPU returns the condition message as the body of a 400,
#' so the message itself is the only channel wide enough to carry more: it is emitted as compact
#' JSON on a single line, keeping it readable to a caller that only takes the first line.
#'
#' The stack is captured by a calling handler, while the erroring frames are still live;
#' `traceback()` after a `tryCatch` would only see the handler. The `run_tool` frames are dropped
#' so the trace starts inside the script.
with_structured_error <- function(expr) {
  traceback <- character(0)
  tryCatch(
    withCallingHandlers(
      expr,
      error = function(e) traceback <<- script_frames(sys.calls())
    ),
    error = function(e) stop(error_payload(e, traceback), call. = FALSE)
  )
}

# Everything the wrapper itself put on the stack is noise to the script author: the frames up to
# and including the `withCallingHandlers` call are this function, and the ones after the last
# script frame are the signalling machinery.
WRAPPER_FRAMES <- "^(stop|\\.handleSimpleError|h|simpleError|function \\(e\\))\\("

script_frames <- function(calls) {
  text <- vapply(calls, deparse_call, character(1))
  start <- which(startsWith(text, "withCallingHandlers("))
  if (length(start)) {
    text <- text[-seq_len(max(start))]
  }
  text <- text[!grepl(WRAPPER_FRAMES, text)]
  utils::tail(text, 50L)
}

deparse_call <- function(call) {
  text <- paste(deparse(call, nlines = 3L), collapse = " ")
  if (nchar(text) > 200L) paste0(substr(text, 1L, 200L), " ...") else text
}

# One line of compact JSON, so a caller reading only the first line of the body still gets the
# whole payload. `error` marks it as structured for a caller that has to tell it apart from the
# plain messages OpenCPU raises before `run_tool` is ever entered (a malformed request body, say).
error_payload <- function(e, traceback) {
  call <- if (is.null(conditionCall(e))) NULL else deparse_call(conditionCall(e))
  payload <- list(
    error = "tool_error",
    message = conditionMessage(e),
    call = call,
    traceback = as.list(traceback)
  )
  as.character(jsonlite::toJSON(payload, auto_unbox = TRUE, null = "null"))
}

#' The runtime a result was produced by: R version, the science packages, and the image build.
runtime_info <- function() {
  pkgs <- c(SCRIPT_PACKAGES, "pracma", "signal", "bigleaf", "jsonlite")
  versions <- lapply(pkgs, function(p) {
    if (requireNamespace(p, quietly = TRUE)) as.character(utils::packageVersion(p)) else NA
  })
  names(versions) <- pkgs
  list(
    r_version = R.version.string,
    packages = versions,
    image_build = Sys.getenv("IMAGE_BUILD_SHA", "unknown")
  )
}

#' The `constants` table shape the portal calculation functions filter on:
#' `constants %>% filter(name == '...') %>% pull('value')`.
constants_df <- function(constants) {
  if (length(constants) == 0L) {
    return(data.frame(name = character(0), value = numeric(0), stringsAsFactors = FALSE))
  }
  data.frame(
    name = names(constants),
    value = as.numeric(unlist(constants, use.names = FALSE)),
    stringsAsFactors = FALSE
  )
}

#' The `standard_curves` row shape the portal functions pull `a`/`b` from. The API sends
#' `slope`/`intercept`; the portal columns are `a` (slope) and `b` (intercept).
curve_df <- function(curve) {
  if (is.null(curve) || length(curve) == 0L) {
    return(NULL)
  }
  data.frame(
    a = as.numeric(curve$slope),
    b = as.numeric(curve$intercept),
    stringsAsFactors = FALSE
  )
}

#' A one-row data frame with the portal's column names, the shape every calculation function
#' takes. NULL entries become NA so missing bench fields read as the portal's empty cells.
row_df <- function(values) {
  cells <- lapply(values, function(v) if (is.null(v) || length(v) == 0L) NA else v)
  as.data.frame(cells, stringsAsFactors = FALSE, check.names = FALSE)
}
