# co2_air: the portal CO2-air tab computes exactly one column, lab_co2air_ch4_dry, from the
# edited raw CO2_air row via calcCH4dry(select(rawData, lab_co2air_h2o, lab_co2air_ch4))
# (modules/tools_tab/tools/co2_air_tool.R:145-155). Every other lab_co2air_* column in the
# category is raw entry the tab echoes back unchanged; nothing else is calculated here.

tool <- function(inputs, constants, curves) {
  num <- function(x) if (is.null(x) || length(x) == 0L) NA_real_ else as.numeric(x)

  results <- list()

  ch4dry <- calcCH4dry(
    riverdata.tools::row_df(list(
      lab_co2air_h2o = num(inputs$h2o_percent),
      lab_co2air_ch4 = num(inputs$ch4_wet)
    ))
  )
  # calcCH4dry returns as.numeric(NA) when a value is missing (utils/calculation_functions.R:682),
  # which is the one case the portal has nothing to show, so it is the one omission. NaN is a
  # value the portal displays and is emitted rather than filtered, so the guard tests NA
  # specifically instead of is.na(), which would swallow NaN with it. calcCH4dry applies no
  # plausibility band to either input and the wrapper adds none.
  if (is.numeric(ch4dry) && length(ch4dry) == 1L && (!is.na(ch4dry) || is.nan(ch4dry))) {
    results$lab_co2air_ch4_dry <- ch4dry
  }

  results
}
