test_that("a script with no entry function names the function it looked for", {
  expect_error(
    run_tool("other <- function(inputs, constants, curves) 1"),
    "tool"
  )
})

test_that("the entry function receives the three lists it was given", {
  script <- "tool <- function(inputs, constants, curves) list(seen = inputs$x * 2)"
  expect_equal(run_tool(script, inputs = list(x = 21))$seen, 42)
})

test_that("an empty script is refused before anything is parsed", {
  expect_error(run_tool(""), "non-empty")
  expect_error(run_tool("tool <- function(...) 1", entry = ""), "non-empty")
})

test_that("a script that raises comes back as one line of JSON carrying the message", {
  script <- "tool <- function(inputs, constants, curves) stop('bench value missing')"
  err <- tryCatch(run_tool(script), error = function(e) conditionMessage(e))
  expect_length(strsplit(err, "\n", fixed = TRUE)[[1]], 1L)
  payload <- jsonlite::fromJSON(err)
  expect_equal(payload$error, "tool_error")
  expect_match(payload$message, "bench value missing")
})

test_that("the traceback starts inside the script, not in the wrapper", {
  script <- paste(
    "helper <- function(v) stop('no')",
    "tool <- function(inputs, constants, curves) helper(inputs$x)",
    sep = "\n"
  )
  err <- tryCatch(run_tool(script, inputs = list(x = 1)), error = function(e) conditionMessage(e))
  trace <- jsonlite::fromJSON(err)$traceback
  expect_true(length(trace) > 0)
  expect_false(any(grepl("withCallingHandlers", trace, fixed = TRUE)))
  expect_true(any(grepl("helper(", trace, fixed = TRUE)))
})

test_that("a syntax error is a tool error too, so an author sees it in the same shape", {
  err <- tryCatch(run_tool("tool <- function( {"), error = function(e) conditionMessage(e))
  expect_equal(jsonlite::fromJSON(err)$error, "tool_error")
})
