# The call structure of a tool script, with line numbers, for the API's safety lint.
#
# R spells one call many ways. `system("ls")`, `system ("ls")`, `` `system`("ls") ``,
# `base::system("ls")`, `do.call("system", list("ls"))`, `get("system")()` and an alias assigned
# first are seven strings and one call. A scan over the source text has to guess at all seven and
# gets the raw string literal `r"(a " b)"` wrong on top; the parse tree has already resolved every
# spelling, which is why the API applies its policy to this rather than to the characters.
#
# This reports structure, not verdicts: which names are refused is the API's list, and it changes
# there alone. Nothing is evaluated, so scanning a hostile script is as safe as reading it.
#
# The runner container is the security boundary: it holds no database credentials, no secrets and
# no network route to anything. The lint this feeds is accident protection, and a determined
# author is not its subject.

# Call heads that are syntax rather than a name a policy could refuse.
SCAN_SYNTAX_CALLS <- INSPECT_SYNTAX_CALLS

# Heads where one side names something rather than evaluating to it, so the two sides cannot be
# walked alike. Each is handled by scan_name_operator.
SCAN_NAME_HEADS <- c("$", "@", "::", ":::")

SCAN_ASSIGN <- c("<-", "<<-", "=")

#' Report every call, symbol reference and argument in a script, each with its line.
#'
#' `calls` carries the head of every call that is not syntax, namespaced heads composed with the
#' operator they were written with (`pkg::fn`, `pkg:::fn`).
#' `symbols` carries every symbol read in value position that the script does not itself bind,
#' which is where an alias (`runner <- system`) shows up, namespaced reads composed the same way
#' as namespaced heads (`runner <- base::system`). `args` carries each argument of those
#' calls as `call`/`name`/`value`/`kind`, so a caller can read `library("curl")` and
#' `cat(f = "out.txt")` without re-parsing anything.
#'
#' A syntax error is a normal return value (`parse_ok = FALSE` plus `parse_error`), matching
#' `parse_check`: a script being typed is unparseable most of the time.
scan_script <- function(script) {
  if (!is.character(script) || length(script) != 1L || is.na(script)) {
    stop("script must be a single string", call. = FALSE)
  }
  parsed <- tryCatch(
    parse(text = script, keep.source = TRUE),
    error = function(e) e
  )
  if (inherits(parsed, "condition")) {
    message <- conditionMessage(parsed)
    position <- regmatches(message, regexpr("[0-9]+:[0-9]+:", message))
    line <- NA_integer_
    column <- NA_integer_
    if (length(position) == 1L) {
      parts <- as.integer(strsplit(sub(":$", "", position), ":", fixed = TRUE)[[1L]])
      line <- parts[1L]
      column <- parts[2L]
    }
    return(list(
      parse_ok = FALSE,
      parse_error = list(message = message, line = line, column = column),
      calls = list(), symbols = list(), args = list()
    ))
  }

  bound <- scan_bound_names(parsed)
  acc <- new.env(parent = emptyenv())
  acc$calls <- list()
  acc$symbols <- list()
  acc$args <- list()

  srcrefs <- attr(parsed, "srcref")
  for (i in seq_along(parsed)) {
    scan_node(parsed[[i]], srcref_line(srcrefs, i, 1L), bound, acc)
  }

  list(
    parse_ok = TRUE,
    parse_error = list(),
    calls = acc$calls,
    symbols = acc$symbols,
    args = acc$args
  )
}

# Only a `{` block carries a srcref per sub-expression, and that is the whole line resolution:
# everything inside one statement inherits the line the statement starts on.
srcref_line <- function(srcrefs, i, fallback) {
  if (is.null(srcrefs) || !is.list(srcrefs) || length(srcrefs) < i) return(fallback)
  ref <- srcrefs[[i]]
  if (is.null(ref)) return(fallback)
  as.integer(ref)[1L]
}

call_head_name <- function(head) {
  if (is.symbol(head)) return(as.character(head))
  if (is.call(head) && length(head) == 3L && is.symbol(head[[1L]]) &&
      as.character(head[[1L]]) %in% c("::", ":::") &&
      is.symbol(head[[2L]]) && is.symbol(head[[3L]])) {
    # The operator is kept: `:::` reaches a package's internals and `::` does not, and only the
    # caller's policy decides whether that matters.
    return(paste0(
      as.character(head[[2L]]), as.character(head[[1L]]), as.character(head[[3L]])
    ))
  }
  NULL
}

# `head_position` says the node is the head of an enclosing call, so whatever it resolves to is
# about to be called rather than read. `f()$g()` and `x[["g"]]()` call `g`; the same two spellings
# in value position are field reads that name no function.
scan_node <- function(node, line, bound, acc, head_position = FALSE) {
  if (!is.call(node)) return(invisible(NULL))
  head <- node[[1L]]
  name <- call_head_name(head)
  head_name <- if (is.null(name)) "" else name
  if (nzchar(head_name) && !head_name %in% SCAN_SYNTAX_CALLS) {
    acc$calls[[length(acc$calls) + 1L]] <- list(name = head_name, line = line)
  }
  if (head_name %in% SCAN_NAME_HEADS) {
    scan_name_operator(node, head_name, line, bound, acc, head_position)
    return(invisible(NULL))
  }
  # `get("system")()` and `f()$g()`: the head is itself a call, and it is the inner one that names
  # what gets called.
  if (is.null(name) && is.call(head)) scan_node(head, line, bound, acc, head_position = TRUE)
  # `x[["system"]]("ls")`: the index is the name being called. In value position the same index is
  # a lookup key, which names no function on its own.
  if (head_position && identical(head_name, "[[") && length(node) == 3L) {
    key <- literal_field(node[[3L]], "[[")
    if (!is.null(key)) acc$calls[[length(acc$calls) + 1L]] <- list(name = key, line = line)
  }

  block_srcrefs <- if (identical(head_name, "{")) attr(node, "srcref") else NULL
  record_args <- nzchar(head_name) && !head_name %in% SCAN_SYNTAX_CALLS
  children <- as.list(node)
  arg_names <- names(children)
  for (i in seq_along(children)[-1L]) {
    if (is_empty_arg(children, i)) next
    child <- children[[i]]
    child_line <- srcref_line(block_srcrefs, i, line)
    arg_name <- if (is.null(arg_names)) "" else arg_names[[i]]
    if (is.na(arg_name)) arg_name <- ""

    if (identical(head_name, "function") && i == 2L) {
      scan_defaults(child, child_line, bound, acc)
      next
    }
    if (record_args) record_arg(acc, head_name, arg_name, child, child_line)
    if (head_name %in% SCAN_ASSIGN && i == 2L && is.symbol(child)) next
    scan_value(child, child_line, bound, acc)
  }
  invisible(NULL)
}

# `$`, `@`, `::` and `:::` each name something on one side, so the sides are walked differently.
#
# The left of `$` and `@` is an ordinary expression: `unlink("scratch")$z` runs the call before it
# indexes the result, so skipping it (as this walk once did for the whole node) lost the plainest
# spellings entirely. The right is a field name and stays unwalked, which is the property this
# operator was given a rule for in the first place: `x$system` reads a field and reaches nothing.
# In head position that flips, because `env$system("ls")` calls whatever `system` names there.
#
# `::` and `:::` name a package and a function, neither of which is an expression to walk. The pair
# is composed into the single name it reaches, so `runner <- base::system` is visible as the
# namespaced name and not as two unrelated symbols. Composition in call position belongs to
# call_head_name, which the caller has already applied.
scan_name_operator <- function(node, operator, line, bound, acc, head_position) {
  if (length(node) != 3L) return(invisible(NULL))
  left <- node[[2L]]
  right <- node[[3L]]
  if (operator %in% c("::", ":::")) {
    if (is.symbol(left) && is.symbol(right)) {
      acc$symbols[[length(acc$symbols) + 1L]] <- list(
        name = paste0(as.character(left), operator, as.character(right)), line = line
      )
      return(invisible(NULL))
    }
    # Not the two names R's grammar allows, so there is no pair to compose; walk both sides rather
    # than report nothing.
    scan_value(left, line, bound, acc)
    scan_value(right, line, bound, acc)
    return(invisible(NULL))
  }
  scan_value(left, line, bound, acc)
  if (head_position) {
    key <- literal_field(right, operator)
    if (!is.null(key)) acc$calls[[length(acc$calls) + 1L]] <- list(name = key, line = line)
  }
  invisible(NULL)
}

# The name in `x$name`, `x@name` or `x[["name"]]`, when it is written out rather than computed.
# `$` and `@` take a bare name (or a string), `[[` a string; a symbol under `[[` is a variable, so
# `x[[key]]` names nothing the tree can report.
literal_field <- function(index, operator) {
  if (operator %in% c("$", "@") && is.symbol(index)) return(as.character(index))
  if (is.character(index) && length(index) == 1L && !is.na(index)) return(index)
  NULL
}

# One child in value position: a symbol is a name the script reads, anything else is walked.
scan_value <- function(child, line, bound, acc) {
  if (is.symbol(child)) {
    value <- as.character(child)
    if (nzchar(value) && !value %in% bound) {
      acc$symbols[[length(acc$symbols) + 1L]] <- list(name = value, line = line)
    }
    return(invisible(NULL))
  }
  scan_node(child, line, bound, acc)
}

# A formal's default is ordinary code (`function(runner = system)`), so it is walked; the formal
# names themselves are bindings, collected by scan_bound_names.
scan_defaults <- function(formals_list, line, bound, acc) {
  if (!is.pairlist(formals_list)) return(invisible(NULL))
  defaults <- as.list(formals_list)
  for (i in seq_along(defaults)) {
    if (is_empty_arg(defaults, i)) next
    scan_value(defaults[[i]], line, bound, acc)
  }
  invisible(NULL)
}

record_arg <- function(acc, call_name, arg_name, arg, line) {
  kind <- "other"
  value <- ""
  if (is.character(arg) && length(arg) == 1L && !is.na(arg)) {
    kind <- "string"
    value <- arg
  } else if (is.symbol(arg)) {
    kind <- "symbol"
    value <- as.character(arg)
  }
  acc$args[[length(acc$args) + 1L]] <- list(
    call = call_name, name = arg_name, value = value, kind = kind, line = line
  )
}

# What the script binds itself. A symbol the script assigns is its own variable, so `file` in
# `path <- file` is a base function only when the script never wrote `file <- ...`.
scan_bound_names <- function(exprs) {
  bound <- character(0)
  stack <- as.list(exprs)
  while (length(stack)) {
    node <- stack[[1L]]
    stack <- stack[-1L]
    if (is.pairlist(node)) {
      names_here <- names(node)
      if (!is.null(names_here)) bound <- c(bound, names_here)
    }
    if (!(is.call(node) || is.pairlist(node))) next
    if (is.call(node) && is.symbol(node[[1L]])) {
      head_name <- as.character(node[[1L]])
      if (head_name %in% SCAN_ASSIGN && length(node) == 3L && is.symbol(node[[2L]])) {
        bound <- c(bound, as.character(node[[2L]]))
      }
      if (identical(head_name, "for") && length(node) >= 2L && is.symbol(node[[2L]])) {
        bound <- c(bound, as.character(node[[2L]]))
      }
    }
    children <- as.list(node)
    keep <- !vapply(seq_along(children), function(i) is_empty_arg(children, i), logical(1))
    stack <- c(children[keep], stack)
  }
  unique(bound)
}
