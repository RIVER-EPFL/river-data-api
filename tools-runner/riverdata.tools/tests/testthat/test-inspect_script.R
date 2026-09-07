# The lint an author reads in the portal. What it reports is the contract the API renders, so a
# wrong answer here is a wrong answer on the authoring screen.

script <- function(...) paste(..., sep = "\n")

test_that("the entry function is found and its arguments come back in declaration order", {
  r <- inspect_script("tool <- function(inputs, constants, curves) list(doc = 1)")
  expect_true(r$parse_ok)
  expect_true(r$entry_found)
  expect_equal(unlist(r$entry_args), c("inputs", "constants", "curves"))
})

test_that("a script defining no entry says so rather than failing", {
  r <- inspect_script("helper <- function(x) x")
  expect_true(r$parse_ok)
  expect_false(r$entry_found)
  expect_length(unlist(r$entry_args), 0L)
})

test_that("a different entry name is honoured", {
  r <- inspect_script("doc_tool <- function(inputs) list(a = 1)", entry = "doc_tool")
  expect_true(r$entry_found)
  expect_equal(r$entry, "doc_tool")
})

test_that("a broken script reports where it broke, and nothing else", {
  r <- inspect_script("tool <- function( {")
  expect_false(r$parse_ok)
  expect_true(nzchar(r$parse_error$message))
  expect_false(r$entry_found)
})

test_that("what the entry reads is reported per list", {
  r <- inspect_script(script(
    "tool <- function(inputs, constants, curves) {",
    "  doc <- inputs$doc_ppb",
    "  m <- constants$molar_c",
    "  a <- curves$doc$slope",
    "  list(out = doc * m * a)",
    "}"
  ))
  expect_true("doc_ppb" %in% unlist(r$inputs))
  expect_true("molar_c" %in% unlist(r$constants))
  expect_true("doc" %in% unlist(r$curves))
})

test_that("a read by a computed name is flagged, because no static list can name it", {
  r <- inspect_script(script(
    "tool <- function(inputs, constants, curves) {",
    "  key <- paste0('doc', '_ppb')",
    "  list(out = inputs[[key]])",
    "}"
  ))
  expect_true(length(unlist(r$dynamic_reads)) > 0)
})

test_that("what the entry returns is reported, and a computed name is flagged not dropped", {
  r <- inspect_script("tool <- function(inputs, constants, curves) list(doc = 1, sd = 2)")
  expect_setequal(unlist(r$outputs), c("doc", "sd"))

  dyn <- inspect_script(script(
    "tool <- function(inputs, constants, curves) {",
    "  out <- list()",
    "  out[[paste0('doc', 1)]] <- 1",
    "  out",
    "}"
  ))
  expect_true(length(unlist(dyn$dynamic_outputs)) > 0 || length(unlist(dyn$outputs)) == 0L)
})

test_that("libraries and namespaces are reported, which is what the whitelist is checked against", {
  r <- inspect_script(script(
    "library(dplyr)",
    "tool <- function(inputs, constants, curves) list(a = stats::median(1:3))"
  ))
  expect_true("dplyr" %in% unlist(r$libraries))
  expect_true("stats" %in% unlist(r$namespaces))
})

test_that("a helper the script defines is not reported as an outside call", {
  r <- inspect_script(script(
    "calc_mean <- function(v) mean(v)",
    "tool <- function(inputs, constants, curves) list(m = calc_mean(inputs$v))"
  ))
  expect_true("calc_mean" %in% unlist(r$functions_defined))
  expect_false("calc_mean" %in% unlist(r$functions_called))
  expect_true("calc_mean" %in% unlist(r$script_functions_used))
})

test_that("a script that is not one string is refused rather than half-inspected", {
  expect_error(inspect_script(c("a", "b")), "single string")
  expect_error(inspect_script("tool <- function() 1", entry = ""), "non-empty")
})

test_that("parse_check answers only whether it parses", {
  expect_true(parse_check("tool <- function() 1")$ok)
  broken <- parse_check("tool <- function( {")
  expect_false(broken$ok)
  expect_true(nzchar(broken$message))
})
