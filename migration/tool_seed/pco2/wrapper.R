# pco2 tool wrapper. Mirrors the portal's pCO2Tool orchestration (pco2_tool.R):
# per replicate calcCH4dry -> calcCO2 -> calcpCO2/calcpCO2P1/calcpCO2P2 -> calcCH4,
# then calcMean/calcSd over the A/B pair. Simple mode applies one calcpCO2 variant
# to an already-computed CO2aq value.

# The prelude functions fetch constants via getRows(pool, 'constants', ...). The API
# resolves constants to values, so the shim hands back the data frame passed as pool.
getRows <- function(pool, ...) pool

tool <- function(inputs, constants, curves) {
  num <- function(v) {
    if (is.null(v) || length(v) == 0L) return(NA_real_)
    v <- suppressWarnings(as.numeric(v[[1]]))
    if (length(v) != 1L) NA_real_ else v
  }
  put <- function(res, key, value) {
    if (is.numeric(value) && length(value) == 1L && is.finite(value)) res[[key]] <- value
    res
  }
  check_band <- function(v, what) {
    if (!is.na(v) && (v < 700 || v > 1050)) {
      stop(sprintf("%s %s hPa is outside the plausible 700-1050 hPa band", what, v))
    }
  }

  mode <- if (is.null(inputs$mode)) "simple" else as.character(inputs$mode)
  variant <- if (is.null(inputs$variant)) "simple" else as.character(inputs$variant)

  water_temp_c <- num(inputs$water_temp_c)
  pressure_hpa <- num(inputs$pressure_hpa)

  # Request volumes override the constants-table vol_sa/vol_water; calcCO2 reads
  # volumes from the constants table only, so the override lands there.
  vol_sa <- num(inputs$vol_sa_ml)
  if (is.na(vol_sa)) vol_sa <- num(constants$vol_sa)
  vol_water <- num(inputs$vol_water_ml)
  if (is.na(vol_water)) vol_water <- num(constants$vol_water)

  pool <- riverdata.tools::constants_df(list(
    c_const = num(constants$c_const),
    gas_const_r_atm = num(constants$gas_const_r_atm),
    gas_const_r_mol = num(constants$gas_const_r_mol),
    h_ch4_29815k = num(constants$h_ch4_29815k),
    ch4_in_sa = num(constants$ch4_in_sa),
    lab_temp_avg_degC = num(constants$lab_temp_avg_degC),
    lab_press_avg_atm = num(constants$lab_press_avg_atm),
    vol_sa = vol_sa,
    vol_water = vol_water
  ))

  res <- list()

  if (mode == "simple") {
    co2_aq <- num(inputs$co2_aq_umol)
    used <- c("mode", "variant", "co2_aq_umol", "water_temp_c")
    if (variant == "simple") {
      df <- riverdata.tools::row_df(list(
        WTW_Temp_degC_1 = water_temp_c, CO2_HS_Um = co2_aq
      ))
      res <- put(res, "pCO2_HS_uatm_avg", calcpCO2(df, pool))
    } else {
      df <- riverdata.tools::row_df(list(
        WTW_Temp_degC_1 = water_temp_c, Field_BP = pressure_hpa,
        Field_BP_altitude = NA_real_, CO2_HS_Um = co2_aq
      ))
      if (variant == "p1") {
        res <- put(res, "pCO2_HS_P1_uatm_avg", calcpCO2P1(df, pool))
      } else {
        res <- put(res, "pCO2_HS_P2_uatm_avg", calcpCO2P2(df, pool))
      }
      used <- c(used, "pressure_hpa")
    }
    res$inputs_used <- used
    return(res)
  }

  # full_pipeline
  lab_temp_c <- num(inputs$lab_temp_c)
  lab_press_hpa <- num(inputs$lab_pressure_hpa)
  check_band(lab_press_hpa, "lab pressure")

  reps <- list(
    A = list(
      co2 = num(inputs$co2_ppm),
      h2o = num(inputs$h2o_percent),
      ch4 = num(inputs$ch4_ppm),
      d13 = num(inputs$d13co2_permil)
    ),
    B = list(
      co2 = num(inputs$replicate_b$co2_ppm),
      h2o = num(inputs$replicate_b$h2o_percent),
      ch4 = num(inputs$replicate_b$ch4_ppm),
      d13 = num(inputs$replicate_b$d13co2_permil)
    )
  )

  per <- list()
  for (rep in c("A", "B")) {
    r <- reps[[rep]]

    ch4_h2o <- riverdata.tools::row_df(setNames(
      list(r$h2o, r$ch4),
      paste0(c("lab_co2_h2o_", "lab_co2_ch4_"), rep)
    ))
    ch4_dry <- calcCH4dry(ch4_h2o)

    co2_df <- riverdata.tools::row_df(setNames(
      list(r$co2, lab_temp_c, lab_press_hpa),
      c(paste0("lab_co2_co2ppm_", rep), "lab_co2_lab_temp", "lab_co2_lab_press")
    ))
    co2_hs <- calcCO2(co2_df, pool)

    p_df <- riverdata.tools::row_df(setNames(
      list(water_temp_c, pressure_hpa, NA_real_, co2_hs),
      c("WTW_Temp_degC_1", "Field_BP", "Field_BP_altitude",
        paste0("CO2_HS_Um_", rep))
    ))
    pco2 <- calcpCO2(p_df, pool)
    pco2_p1 <- calcpCO2P1(p_df, pool)
    pco2_p2 <- calcpCO2P2(p_df, pool)

    ch4_df <- riverdata.tools::row_df(setNames(
      list(lab_temp_c, lab_press_hpa, water_temp_c, pressure_hpa, NA_real_, ch4_dry),
      c("lab_co2_lab_temp", "lab_co2_lab_press", "WTW_Temp_degC_1",
        "Field_BP", "Field_BP_altitude", paste0("lab_co2_ch4_dry_", rep))
    ))
    ch4_dissolved <- calcCH4(ch4_df, pool)

    res <- put(res, paste0("lab_co2_ch4_dry_", rep), ch4_dry)
    res <- put(res, paste0("CH4_calc_umol_L_", rep), ch4_dissolved)
    res <- put(res, paste0("CO2_HS_Um_", rep), co2_hs)
    res <- put(res, paste0("pCO2_HS_uatm_", rep), pco2)
    res <- put(res, paste0("pCO2_HS_P1_uatm_", rep), pco2_p1)
    res <- put(res, paste0("pCO2_HS_P2_uatm_", rep), pco2_p2)

    per[[rep]] <- list(
      co2_hs = co2_hs, pco2 = pco2, pco2_p1 = pco2_p1, pco2_p2 = pco2_p2,
      ch4 = ch4_dissolved, d13 = r$d13
    )
  }

  pair <- function(field) {
    riverdata.tools::row_df(list(A = per$A[[field]], B = per$B[[field]]))
  }
  agg <- list(
    CO2_HS_Um = pair("co2_hs"),
    pCO2_HS_uatm = pair("pco2"),
    pCO2_HS_P1_uatm = pair("pco2_p1"),
    pCO2_HS_P2_uatm = pair("pco2_p2"),
    d13C_CO2 = pair("d13"),
    CH4_umol_L = pair("ch4")
  )
  for (base in names(agg)) {
    res <- put(res, paste0(base, "_avg"), calcMean(agg[[base]]))
    res <- put(res, paste0(base, "_sd"), calcSd(agg[[base]]))
  }

  used <- c("mode", "co2_ppm", "h2o_percent", "ch4_ppm", "water_temp_c", "pressure_hpa")
  for (name in c("d13co2_permil", "lab_temp_c", "lab_pressure_hpa",
                 "vol_sa_ml", "vol_water_ml", "replicate_b")) {
    if (!is.null(inputs[[name]])) used <- c(used, name)
  }
  res$inputs_used <- used
  res
}
