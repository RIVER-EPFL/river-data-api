test_that("a curve arrives as the portal's a and b columns", {
  df <- curve_df(list(slope = 2.5, intercept = -0.25))
  expect_equal(df$a, 2.5)
  expect_equal(df$b, -0.25)
})

test_that("no curve is no table, which is what a calculation checks for", {
  expect_null(curve_df(NULL))
  expect_null(curve_df(list()))
})

test_that("a numeric string is a number, as the portal stored them", {
  df <- curve_df(list(slope = "2.5", intercept = "-0.25"))
  expect_equal(df$a, 2.5)
  expect_equal(df$b, -0.25)
})

test_that("a non-numeric coefficient is refused by name rather than becoming NA", {
  # as.numeric("steep") is NA with a warning, and a calculation carries NA into its output.
  expect_error(curve_df(list(slope = "steep", intercept = 0.1)), "slope")
  expect_error(curve_df(list(slope = 1.0, intercept = "zero")), "intercept")
})

test_that("a missing coefficient is refused by name", {
  expect_error(curve_df(list(intercept = 0.1)), "slope")
  expect_error(curve_df(list(slope = 1.0)), "intercept")
  expect_error(curve_df(list(slope = NULL, intercept = 0.1)), "slope")
})
