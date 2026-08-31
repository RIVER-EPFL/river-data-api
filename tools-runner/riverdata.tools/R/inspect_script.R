# Static inspection of a tool script: what it reads, what it returns, what it depends on.
#
# Nothing here evaluates the script. `parse()` builds the AST and the walkers below read it, so
# inspecting a hostile or half-written script is as safe as reading it. That is the whole point:
# the portal inspects while an author types, long before the script is runnable.

# Call heads that are syntax rather than a dependency: reporting them as "functions this script
# calls" would bury the two prelude functions a wrapper actually uses.
INSPECT_SYNTAX_CALLS <- c(
  "+", "-", "*", "/", "^", "%%", "%/%", "%*%", "%in%", "%o%", "%>%",
  "==", "!=", "<", ">", "<=", ">=", "!", "&", "&&", "|", "||",
  "(", "{", "[", "[[", "$", "@", ":", "::", ":::", "~", "?",
  "<-", "<<-", "=", "->", "->>", "if", "for", "while", "repeat", "function",
  "return", "break", "next", "missing", "on.exit"
)

# The three lists a tool script is handed. Reads of each are reported separately, because the
# manifest declares them separately.
INSPECT_SOURCES <- c("inputs", "constants", "curves")

#' Report what a tool script reads and what it looks like it returns, without running it.
#'
#' `script` is the complete R source of one tool (the shared prelude plus a wrapper) and `entry`
#' names the function the API would call. The result is the raw material for a tool manifest:
#' the `inputs$`/`constants$`/`curves$` names the script reads, the keys its entry function
#' returns, the functions it depends on, and the `library()` calls the API lints.
#'
#' A syntax error is a normal return value (`parse_ok = FALSE` plus `parse_error`), not a
#' condition: the portal renders it as inline feedback while someone is still typing, and an
#' unparseable script is the expected state for most of that time.
#'
#' Detection is static, so it is deliberately incomplete rather than confident. Names built at
#' runtime (`out[[paste0(base, "_", rep)]] <- ...`) cannot be known from the AST; they are
#' reported through `dynamic_outputs` so a caller reads the static list as a floor, never as the
#' complete set.
inspect_script <- function(script, entry = "tool") {
  if (!is.character(script) || length(script) != 1L || is.na(script)) {
    stop("script must be a single string", call. = FALSE)
  }
  if (!is.character(entry) || length(entry) != 1L || !nzchar(entry)) {
    stop("entry must be a single non-empty string", call. = FALSE)
  }

  parsed <- parse_script(script)
  if (!parsed$ok) {
    return(c(list(parse_ok = FALSE, parse_error = parsed$error), empty_report(entry)))
  }

  top_exprs <- as.list(parsed$exprs)
  nodes <- flatten_ast(top_exprs)
  reads <- collect_reads(nodes)
  defined <- collect_defined_functions(nodes, top_exprs)
  called <- collect_called(nodes)
  entry_def <- find_function_def(top_exprs, entry)

  report <- list(
    parse_ok = TRUE,
    parse_error = list(),
    entry = entry,
    entry_found = !is.null(entry_def),
    # Declaration order, not sorted: the API calls the entry positionally.
    entry_args = as.list(if (is.null(entry_def)) character(0) else formal_names(entry_def)),
    inputs = str_list(reads$inputs),
    constants = str_list(reads$constants),
    curves = str_list(reads$curves),
    outputs = str_list(character(0)),
    dynamic_outputs = flag_list(character(0)),
    dynamic_reads = flag_list(reads$dynamic),
    functions_defined = str_list(defined$all),
    functions_called = str_list(setdiff(called, defined$all)),
    script_functions_used = str_list(character(0)),
    libraries = str_list(collect_libraries(nodes)),
    namespaces = str_list(collect_namespaces(nodes))
  )

  if (!is.null(entry_def)) {
    body_expr <- entry_def[[3L]]
    outs <- collect_outputs(body_expr)
    report$outputs <- str_list(outs$names)
    report$dynamic_outputs <- flag_list(outs$dynamic)
    entry_nodes <- flatten_ast(list(body_expr))
    report$script_functions_used <- str_list(intersect(collect_called(entry_nodes), defined$top_level))
  }
  report
}

#' Syntax-check a script and nothing else.
#'
#' Returns `list(ok = TRUE)`, or `ok = FALSE` with the parse message and the line and column it
#' points at when R reports them. The API's script lint has no other way to tell a broken script
#' from a working one before storing it.
parse_check <- function(script) {
  if (!is.character(script) || length(script) != 1L || is.na(script)) {
    stop("script must be a single string", call. = FALSE)
  }
  parsed <- parse_script(script)
  if (parsed$ok) list(ok = TRUE) else c(list(ok = FALSE), parsed$error)
}

# ---- parsing -----------------------------------------------------------------------------

# `keep.source = FALSE` because nothing here needs srcrefs and they double the walk. The parse
# message already carries the position: R formats it as "<script>:LINE:COL: unexpected ...".
parse_script <- function(script) {
  exprs <- tryCatch(
    parse(text = script, keep.source = FALSE, srcfile = NULL),
    error = function(e) e
  )
  if (!inherits(exprs, "condition")) {
    return(list(ok = TRUE, exprs = exprs, error = list()))
  }
  message <- conditionMessage(exprs)
  position <- regmatches(message, regexpr("[0-9]+:[0-9]+:", message))
  line <- NA_integer_
  column <- NA_integer_
  if (length(position) == 1L) {
    parts <- as.integer(strsplit(sub(":$", "", position), ":", fixed = TRUE)[[1L]])
    line <- parts[1L]
    column <- parts[2L]
  }
  list(
    ok = FALSE,
    exprs = NULL,
    error = list(message = message, line = line, column = column)
  )
}

# ---- AST walking -------------------------------------------------------------------------

# Every node of every expression, flattened once so each collector is a single pass over a list
# rather than its own recursion. A missing argument (the empty symbol in `df[, 1]`) is dropped:
# forcing one raises "argument is missing", so it is filtered by comparing the unextracted
# sublist, never the element.
flatten_ast <- function(exprs) {
  out <- list()
  stack <- exprs
  while (length(stack)) {
    node <- stack[[1L]]
    stack <- stack[-1L]
    if (!(is.call(node) || is.pairlist(node) || is.expression(node))) next
    out[[length(out) + 1L]] <- node
    children <- as.list(node)
    keep <- !vapply(seq_along(children), function(i) is_empty_arg(children, i), logical(1))
    stack <- c(children[keep], stack)
  }
  out
}

# Names are stripped first: a formal with no default (`function(df, ...)`) is an empty symbol
# under its own name, and a named sublist never matches the bare one.
is_empty_arg <- function(lst, i) identical(unname(lst[i]), list(quote(expr = )))

# ---- what the script reads ---------------------------------------------------------------

# `inputs$name`, `inputs[["name"]]` and `inputs[['name']]` are the same read; `inputs[[key]]` is
# a read whose name only exists at runtime, so it is reported as dynamic rather than guessed at.
collect_reads <- function(nodes) {
  found <- list(inputs = character(0), constants = character(0), curves = character(0))
  dynamic <- character(0)
  for (node in nodes) {
    if (!is.call(node) || length(node) < 3L) next
    head <- node[[1L]]
    if (!is.symbol(head) || !as.character(head) %in% c("$", "[[")) next
    target <- node[[2L]]
    if (!is.symbol(target)) next
    source <- as.character(target)
    if (!source %in% INSPECT_SOURCES) next
    key <- literal_name(node[[3L]], as.character(head))
    if (is.null(key)) {
      dynamic <- c(dynamic, deparse_call(node))
    } else {
      found[[source]] <- c(found[[source]], key)
    }
  }
  list(
    inputs = tidy(found$inputs),
    constants = tidy(found$constants),
    curves = tidy(found$curves),
    dynamic = tidy(dynamic)
  )
}

# `$` takes a bare name, `[[` a string. A symbol under `[[` is a variable, not a name.
literal_name <- function(index, operator) {
  if (identical(operator, "$")) {
    if (is.symbol(index)) return(as.character(index))
    if (is.character(index) && length(index) == 1L) return(index)
    return(NULL)
  }
  if (is.character(index) && length(index) == 1L && !is.na(index)) return(index)
  NULL
}

# ---- definitions, calls, libraries -------------------------------------------------------

# Two sets, because they answer different questions. `all` (top level and nested) is what
# `functions_called` subtracts, so a wrapper's own `num()` helper is not reported as a
# dependency; `top_level` is the prelude's catalogue, which is what a tool depends *on*.
collect_defined_functions <- function(nodes, top_exprs) {
  all <- character(0)
  for (node in nodes) {
    name <- assigned_function_name(node)
    if (!is.null(name)) all <- c(all, name)
  }
  top <- character(0)
  for (expr in top_exprs) {
    name <- assigned_function_name(expr)
    if (!is.null(name)) top <- c(top, name)
  }
  list(all = tidy(all), top_level = tidy(top))
}

assigned_function_name <- function(node) {
  if (!is.call(node) || length(node) != 3L) return(NULL)
  head <- node[[1L]]
  if (!is.symbol(head) || !as.character(head) %in% c("<-", "<<-", "=")) return(NULL)
  if (!is.symbol(node[[2L]])) return(NULL)
  if (!is_function_literal(node[[3L]])) return(NULL)
  as.character(node[[2L]])
}

is_function_literal <- function(node) {
  is.call(node) && is.symbol(node[[1L]]) && as.character(node[[1L]]) == "function"
}

# A namespaced call keeps its namespace ("riverdata.tools::row_df"): which package a tool reaches
# into is part of what the manifest has to declare.
collect_called <- function(nodes) {
  called <- character(0)
  for (node in nodes) {
    if (!is.call(node)) next
    head <- node[[1L]]
    if (is.symbol(head)) {
      name <- as.character(head)
      if (!name %in% INSPECT_SYNTAX_CALLS) called <- c(called, name)
      next
    }
    if (is.call(head) && is.symbol(head[[1L]]) &&
        as.character(head[[1L]]) %in% c("::", ":::") && length(head) == 3L) {
      called <- c(called, paste0(as.character(head[[2L]]), "::", as.character(head[[3L]])))
    }
  }
  tidy(called)
}

collect_libraries <- function(nodes) {
  libs <- character(0)
  for (node in nodes) {
    if (!is.call(node) || !is.symbol(node[[1L]]) || length(node) < 2L) next
    if (!as.character(node[[1L]]) %in% c("library", "require", "requireNamespace")) next
    arg <- node[[2L]]
    if (is.symbol(arg)) libs <- c(libs, as.character(arg))
    if (is.character(arg) && length(arg) == 1L) libs <- c(libs, arg)
  }
  tidy(libs)
}

collect_namespaces <- function(nodes) {
  spaces <- character(0)
  for (node in nodes) {
    if (!is.call(node) || length(node) != 3L || !is.symbol(node[[1L]])) next
    if (!as.character(node[[1L]]) %in% c("::", ":::")) next
    if (is.symbol(node[[2L]])) spaces <- c(spaces, as.character(node[[2L]]))
  }
  tidy(spaces)
}

# ---- the entry function and its outputs ---------------------------------------------------

find_function_def <- function(exprs, name) {
  for (expr in exprs) {
    if (is.null(assigned_function_name(expr))) next
    if (as.character(expr[[2L]]) != name) next
    return(expr[[3L]])
  }
  NULL
}

formal_names <- function(fn_literal) {
  formals_list <- fn_literal[[2L]]
  if (is.null(formals_list)) return(character(0))
  names <- names(as.list(formals_list))
  if (is.null(names)) character(0) else names
}

# The expressions whose value the entry function yields: the tail of its body, plus every
# `return()` that is not inside a nested closure (one of those returns from the closure).
return_expressions <- function(body_expr) {
  tails <- tail_expressions(body_expr)
  returns <- list()
  stack <- list(body_expr)
  while (length(stack)) {
    node <- stack[[1L]]
    stack <- stack[-1L]
    if (!(is.call(node) || is.pairlist(node))) next
    if (is_function_literal(node)) next
    if (is.symbol(node[[1L]]) && as.character(node[[1L]]) == "return" && length(node) >= 2L) {
      returns[[length(returns) + 1L]] <- node[[2L]]
      next
    }
    children <- as.list(node)
    keep <- !vapply(seq_along(children), function(i) is_empty_arg(children, i), logical(1))
    stack <- c(children[keep], stack)
  }
  c(tails, returns)
}

# A `{` block yields its last expression, an `if` yields either branch.
tail_expressions <- function(node) {
  if (!is.call(node)) return(list(node))
  head <- if (is.symbol(node[[1L]])) as.character(node[[1L]]) else ""
  if (head == "{") {
    if (length(node) == 1L) return(list())
    return(tail_expressions(node[[length(node)]]))
  }
  if (head == "(") return(tail_expressions(node[[2L]]))
  if (head == "if") {
    branches <- tail_expressions(node[[3L]])
    if (length(node) >= 4L) branches <- c(branches, tail_expressions(node[[4L]]))
    return(branches)
  }
  list(node)
}

collect_outputs <- function(body_expr) {
  ctx <- list(
    body = body_expr,
    helpers = local_helpers(body_expr),
    names = character(0),
    dynamic = character(0)
  )
  for (expr in return_expressions(body_expr)) {
    ctx <- resolve_output(expr, ctx, seen = character(0))
  }
  list(names = tidy(ctx$names), dynamic = tidy(ctx$dynamic))
}

# What one returned expression contributes. `list(a = ..., b = ...)` names itself; `c(x, y)`
# hands off to each part; a bare symbol is a variable the body built up. Anything else is
# reported as dynamic rather than guessed at.
resolve_output <- function(expr, ctx, seen) {
  if (is.symbol(expr)) {
    return(resolve_variable(as.character(expr), ctx, seen))
  }
  if (!is.call(expr)) {
    return(mark_dynamic(ctx, expr))
  }
  head <- if (is.symbol(expr[[1L]])) as.character(expr[[1L]]) else ""
  if (head == "list") {
    args <- as.list(expr)[-1L]
    arg_names <- names(args)
    if (is.null(arg_names)) arg_names <- rep("", length(args))
    for (i in seq_along(args)) {
      if (nzchar(arg_names[i])) {
        ctx$names <- c(ctx$names, arg_names[i])
      } else {
        ctx <- mark_dynamic(ctx, args[[i]])
      }
    }
    return(ctx)
  }
  if (head %in% c("c", "modifyList", "utils::modifyList")) {
    for (arg in as.list(expr)[-1L]) ctx <- resolve_output(arg, ctx, seen)
    return(ctx)
  }
  if (head == "(") return(resolve_output(expr[[2L]], ctx, seen))
  mark_dynamic(ctx, expr)
}

# Every assignment anywhere in the entry body that writes into `name`, including the ones a local
# closure makes with `<<-`. Scanning the whole body rather than the top level is what finds
# `out[[key]] <<- val` inside a `put()` helper, which is how several tools build their result.
resolve_variable <- function(name, ctx, seen) {
  if (name %in% seen) return(ctx)
  seen <- c(seen, name)
  for (node in flatten_ast(list(ctx$body))) {
    if (!is.call(node) || length(node) != 3L || !is.symbol(node[[1L]])) next
    if (!as.character(node[[1L]]) %in% c("<-", "<<-", "=")) next
    target <- node[[2L]]
    value <- node[[3L]]

    if (is.symbol(target) && as.character(target) == name) {
      ctx <- resolve_assignment_value(value, ctx, seen)
      next
    }
    if (!is.call(target) || length(target) != 3L || !is.symbol(target[[1L]])) next
    operator <- as.character(target[[1L]])
    if (!operator %in% c("$", "[[")) next
    if (!is.symbol(target[[2L]]) || as.character(target[[2L]]) != name) next

    key <- literal_name(target[[3L]], operator)
    if (!is.null(key)) {
      ctx$names <- c(ctx$names, key)
      next
    }
    ctx <- resolve_key_expression(target[[3L]], node, ctx)
  }
  ctx
}

# `out <- <something>`. A helper call that accumulates into its own argument (`res <- put(res,
# "k", v)`) is followed into the helper; anything else is resolved as a returned expression.
resolve_assignment_value <- function(value, ctx, seen) {
  if (is.call(value) && is.symbol(value[[1L]])) {
    helper <- ctx$helpers[[as.character(value[[1L]])]]
    if (!is.null(helper) && length(helper$key_formals)) {
      return(resolve_helper_call(value, helper, ctx))
    }
  }
  resolve_output(value, ctx, seen)
}

# A non-literal key. When the key is a formal of the closure the assignment sits in, the actual
# names are at that closure's call sites, so they are recovered from there; when a call site
# passes anything but a string literal, that one call stays dynamic.
resolve_key_expression <- function(key_expr, assignment, ctx) {
  if (is.symbol(key_expr)) {
    owner <- helper_owning(ctx$helpers, assignment)
    if (!is.null(owner) && as.character(key_expr) %in% owner$formals) {
      return(resolve_helper_sites(as.character(key_expr), owner, ctx))
    }
  }
  mark_dynamic(ctx, assignment)
}

# ---- local helper closures ----------------------------------------------------------------

# The closures the entry function defines, with the formals each one uses as an output key.
local_helpers <- function(body_expr) {
  helpers <- list()
  for (node in flatten_ast(list(body_expr))) {
    name <- assigned_function_name(node)
    if (is.null(name)) next
    definition <- node[[3L]]
    formals <- formal_names(definition)
    helpers[[name]] <- list(
      name = name,
      formals = formals,
      body = definition[[3L]],
      key_formals = key_formals(definition[[3L]], formals),
      calls = list()
    )
  }
  if (!length(helpers)) return(helpers)
  for (node in flatten_ast(list(body_expr))) {
    if (!is.call(node) || !is.symbol(node[[1L]])) next
    name <- as.character(node[[1L]])
    if (is.null(helpers[[name]])) next
    helpers[[name]]$calls <- c(helpers[[name]]$calls, list(node))
  }
  helpers
}

# Formals used as `target[[formal]] <- ...` inside the closure, whatever the target: a free
# variable reached with `<<-`, or a formal the closure accumulates into and returns.
key_formals <- function(body_expr, formals) {
  keys <- character(0)
  for (node in flatten_ast(list(body_expr))) {
    if (!is.call(node) || length(node) != 3L || !is.symbol(node[[1L]])) next
    if (!as.character(node[[1L]]) %in% c("<-", "<<-", "=")) next
    target <- node[[2L]]
    if (!is.call(target) || length(target) != 3L || !is.symbol(target[[1L]])) next
    if (!as.character(target[[1L]]) %in% c("$", "[[")) next
    index <- target[[3L]]
    if (is.symbol(index) && as.character(index) %in% formals) keys <- c(keys, as.character(index))
  }
  unique(keys)
}

helper_owning <- function(helpers, assignment) {
  for (helper in helpers) {
    for (node in flatten_ast(list(helper$body))) {
      if (identical(node, assignment)) return(helper)
    }
  }
  NULL
}

# Every call site of the closure, read for the argument that lands in the key formal.
resolve_helper_sites <- function(formal, helper, ctx) {
  if (!length(helper$calls)) {
    return(mark_dynamic(ctx, sprintf("%s(%s = ?) is never called", helper$name, formal)))
  }
  for (site in helper$calls) {
    ctx <- resolve_helper_argument(site, helper, formal, ctx)
  }
  ctx
}

resolve_helper_call <- function(site, helper, ctx) {
  for (formal in helper$key_formals) {
    ctx <- resolve_helper_argument(site, helper, formal, ctx)
  }
  ctx
}

resolve_helper_argument <- function(site, helper, formal, ctx) {
  arg <- match_argument(site, helper$formals, formal)
  if (is.null(arg)) return(mark_dynamic(ctx, site))
  if (is.character(arg) && length(arg) == 1L && !is.na(arg)) {
    ctx$names <- c(ctx$names, arg)
    return(ctx)
  }
  mark_dynamic(ctx, site)
}

# Argument matching by hand: exact names first, then the unnamed arguments in order over the
# formals still free. `match.call` would need the closure itself, and building one means
# evaluating part of the script.
match_argument <- function(site, formals, wanted) {
  args <- as.list(site)[-1L]
  if (!length(args)) return(NULL)
  arg_names <- names(args)
  if (is.null(arg_names)) arg_names <- rep("", length(args))
  named <- which(nzchar(arg_names))
  for (i in named) {
    if (identical(arg_names[i], wanted)) return(args[[i]])
  }
  free <- setdiff(formals, c(arg_names[named], "..."))
  position <- match(wanted, free)
  if (is.na(position)) return(NULL)
  positional <- args[!nzchar(arg_names)]
  if (position > length(positional)) return(NULL)
  if (is_empty_arg(positional, position)) return(NULL)
  positional[[position]]
}

# ---- small shared pieces -------------------------------------------------------------------

mark_dynamic <- function(ctx, expr) {
  text <- if (is.character(expr)) expr else deparse_call(expr)
  ctx$dynamic <- c(ctx$dynamic, text)
  ctx
}

# `any` is what a caller branches on; the expressions are what it shows when it does.
flag_list <- function(expressions) {
  list(any = length(expressions) > 0L, expressions = str_list(expressions))
}

# Every name list leaves here as a list, never a character vector: the API calls this endpoint
# with `auto_unbox=true`, which turns a one-element vector into a bare string, and a field that
# is sometimes `["x"]` and sometimes `"x"` has to be decoded twice. An empty list is `[]`.
str_list <- function(x) as.list(tidy(x))

# Radix order, not the locale's: a caller diffing two inspections should see a change in the
# script, never a change in the container's collation.
tidy <- function(x) {
  if (!length(x)) return(character(0))
  sort(unique(as.character(x)), method = "radix")
}

# The report keeps its shape when the script does not parse, so a caller reads the same fields
# whatever it sent.
empty_report <- function(entry) {
  empty <- str_list(character(0))
  list(
    entry = entry,
    entry_found = FALSE,
    entry_args = empty,
    inputs = empty,
    constants = empty,
    curves = empty,
    outputs = empty,
    dynamic_outputs = flag_list(character(0)),
    dynamic_reads = flag_list(character(0)),
    functions_defined = empty,
    functions_called = empty,
    script_functions_used = empty,
    libraries = empty,
    namespaces = empty
  )
}
