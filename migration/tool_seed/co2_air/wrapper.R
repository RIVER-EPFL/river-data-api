# co2_air: CH4 dry concentration via calcCH4dry; dissolved headspace CO2 from the lab CO2
# entry via calcCO2 (default mode: entered lab temp/pressure win, blanks fall back to the
# constants table). Entered volumes replace the vol_sa/vol_water constants before calcCO2.

getRows <- function(pool, table, ...) pool[[table]]

tool <- function(inputs, constants, curves) {
  num <- function(x) if (is.null(x) || length(x) == 0L) NA_real_ else as.numeric(x)
  finite1 <- function(x) is.numeric(x) && length(x) == 1L && is.finite(x)

  results <- list()

  ch4 <- calcCH4dry(riverdata.tools::row_df(list(
    lab_co2air_h2o = num(inputs$h2o_percent),
    lab_co2air_ch4 = num(inputs$ch4_wet)
  )))
  if (finite1(ch4)) results$lab_co2air_ch4_dry <- ch4

  co2ppm <- num(inputs$co2_ppm)
  if (!is.na(co2ppm)) {
    cst <- constants
    vol_sa <- num(inputs$vol_sa_ml)
    vol_water <- num(inputs$vol_water_ml)
    if (!is.na(vol_sa)) cst$vol_sa <- vol_sa
    if (!is.na(vol_water)) cst$vol_water <- vol_water
    pool <- list(constants = riverdata.tools::constants_df(cst))
    co2 <- calcCO2(
      riverdata.tools::row_df(list(
        lab_co2_lab_temp = num(inputs$lab_temp_c),
        lab_co2_lab_press = num(inputs$lab_pressure_hpa),
        lab_co2_co2ppm = co2ppm
      )),
      pool
    )
    if (finite1(co2)) results$CO2_HS_Um <- co2
  }

  results
}
