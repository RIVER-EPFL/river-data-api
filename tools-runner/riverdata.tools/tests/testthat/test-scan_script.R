# The call structure the API's safety lint applies its policy to. The policy is Rust's and is tested
# there against hand-built scans; this is the extraction those scans stand for, so a spelling lost
# here lints clean however the policy reads.

script <- function(...) paste(..., sep = "\n")

calls_of <- function(r) vapply(r$calls, function(c) c$name, character(1))
symbols_of <- function(r) vapply(r$symbols, function(s) s$name, character(1))
line_of <- function(entries, name) {
  hits <- Filter(function(e) identical(e$name, name), entries)
  if (length(hits) == 0L) return(NA_integer_)
  hits[[1L]]$line
}
string_args <- function(r, call) {
  hits <- Filter(function(a) identical(a$call, call) && identical(a$kind, "string"), r$args)
  vapply(hits, function(a) a$value, character(1))
}

test_that("a plain call is reported by its head, at its line", {
  r <- scan_script(script("x <- 1", "system(\"ls\")"))
  expect_true(r$parse_ok)
  expect_true("system" %in% calls_of(r))
  expect_equal(line_of(r$calls, "system"), 2L)
})

test_that("a space before the parenthesis is the same call", {
  r <- scan_script("system (\"ls\")")
  expect_true("system" %in% calls_of(r))
})

test_that("a backquoted head is the same call", {
  r <- scan_script("`system`(\"ls\")")
  expect_true("system" %in% calls_of(r))
})

test_that("a namespaced head keeps the operator it was written with", {
  r <- scan_script(script("base::system(\"ls\")", "base:::system(\"ls\")"))
  expect_equal(line_of(r$calls, "base::system"), 1L)
  expect_equal(line_of(r$calls, "base:::system"), 2L)
  expect_false("system" %in% calls_of(r))
})

test_that("do.call names its target as a string argument", {
  r <- scan_script("do.call(\"system\", list(\"ls\"))")
  expect_true("do.call" %in% calls_of(r))
  expect_true("system" %in% string_args(r, "do.call"))
})

test_that("get()() reports the get and the name it fetches", {
  r <- scan_script("get(\"system\")(\"ls\")")
  expect_true("get" %in% calls_of(r))
  expect_true("system" %in% string_args(r, "get"))
})

test_that("an alias assigned before use shows the aliased name as a symbol read", {
  r <- scan_script(script("runner <- system", "runner(\"ls\")"))
  expect_equal(line_of(r$symbols, "system"), 1L)
  expect_equal(line_of(r$calls, "runner"), 2L)
  expect_false("runner" %in% symbols_of(r))
})

test_that("a namespaced alias is composed into the one name it reaches", {
  r <- scan_script(script("runner <- base::system", "runner(\"ls\")"))
  expect_true("base::system" %in% symbols_of(r))
  expect_false("base" %in% symbols_of(r))
})

test_that("a name the script binds itself is not reported as a read", {
  r <- scan_script(script("file <- 'a.csv'", "path <- file"))
  expect_false("file" %in% symbols_of(r))
})

test_that("a call through a field or an index names the field", {
  r <- scan_script(script("env$system(\"ls\")", "env[[\"system\"]](\"ls\")"))
  expect_equal(line_of(r$calls, "system"), 1L)
  expect_equal(sum(calls_of(r) == "system"), 2L)
})

test_that("a field read in value position names no call", {
  r <- scan_script("x <- env$system")
  expect_false("system" %in% calls_of(r))
  expect_false("system" %in% symbols_of(r))
})

test_that("a raw string literal is a string, not the code it looks like", {
  r <- scan_script("cat(r\"(system(\"ls\") \" b)\")")
  expect_equal(calls_of(r), "cat")
  expect_equal(string_args(r, "cat"), "system(\"ls\") \" b")
})

test_that("each statement of a function body carries its own line", {
  r <- scan_script(script(
    "tool <- function(inputs, constants, curves) {",
    "  x <- inputs$a",
    "  unlink(\"scratch\")",
    "  list(out = x)",
    "}"
  ))
  expect_equal(line_of(r$calls, "unlink"), 3L)
  expect_equal(line_of(r$calls, "list"), 4L)
})

test_that("a default argument is walked as code", {
  r <- scan_script("tool <- function(inputs, runner = system) runner('ls')")
  expect_true("system" %in% symbols_of(r))
})

test_that("a syntax error is a return value with its position", {
  r <- scan_script(script("x <- 1", "tool <- function( {"))
  expect_false(r$parse_ok)
  expect_true(nzchar(r$parse_error$message))
  expect_equal(r$parse_error$line, 2L)
  expect_length(r$calls, 0L)
})

test_that("anything but one string is refused", {
  expect_error(scan_script(c("a", "b")), "single string")
  expect_error(scan_script(NA_character_), "single string")
})
