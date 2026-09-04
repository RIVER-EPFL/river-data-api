test_that("every constant keeps its own name", {
  df <- constants_df(list(molar_c = 12.011, molar_o = 15.999, na_to_mg = 22.99))
  expect_equal(df$name, c("molar_c", "molar_o", "na_to_mg"))
  expect_equal(df$value, c(12.011, 15.999, 22.99))
})

test_that("no constants is an empty table with the portal's columns", {
  df <- constants_df(list())
  expect_equal(names(df), c("name", "value"))
  expect_equal(nrow(df), 0L)
  expect_type(df$value, "double")
})

test_that("a null constant is refused by name rather than shifting the ones after it", {
  # JSON null parses to a NULL list entry, which unlist() drops silently.
  constants <- list(molar_c = 12.011, molar_o = NULL, na_to_mg = 22.99)
  expect_error(constants_df(constants), "molar_o")
})

test_that("a non-numeric constant is refused by name", {
  expect_error(constants_df(list(molar_c = 12.011, label = "carbon")), "label")
})

test_that("an unnamed constant is refused", {
  expect_error(constants_df(list(12.011, molar_o = 15.999)))
})

test_that("a numeric string is a number, as the portal stored them", {
  df <- constants_df(list(molar_c = "12.011"))
  expect_equal(df$value, 12.011)
})
