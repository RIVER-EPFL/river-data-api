# pco2 tool wrapper. Mirrors the portal's pCO2Tool orchestration
# (modules/tools_tab/tools/pco2_tool.R):
#   for rep in c('A','B'): calcCH4dry -> calcCO2 -> calcCH4 / calcpCO2 /
#   calcpCO2P1 / calcpCO2P2, then calcMean/calcSd over the A/B pair for
#   CO2_HS_Um, pCO2_HS_uatm, pCO2_HS_P1_uatm, pCO2_HS_P2_uatm, d13C_CO2, CH4.
#
# The portal has one control flow only: pco2_tool.R:207 loops both replicates
# unconditionally and :280-297 averages the pair. There is no single-value
# entry point, so this wrapper has none either.
#
# Lab temperature and lab pressure are the portal's two checkboxes
# (pco2_tool.R:28-29, 194-204): checked -> 'cst' (constants table), unchecked ->
# 'db' (the value typed into the lab-constant table). The portal never uses
# calcCO2/calcCH4's 'default' mode, so a blank db value yields NA rather than
# silently falling back to the constant.
#
# Field barometric pressure and its altitude-derived fallback are the row's
# Field_BP / Field_BP_altitude (pco2_tool.R:191); calcCH4, calcpCO2P1 and
# calcpCO2P2 pick Field_BP when it is present and within 700-1050 hPa and fall
# back to Field_BP_altitude otherwise. No band is applied to lab pressure.
#
# The four Picarro measurements are read per replicate by name
# (lab_co2_co2ppm_rep_A ... lab_co2_ico2_rep_B), which are the portal's own
# columns (lab_co2_co2ppm_A etc, selected at pco2_tool.R:209-210 and :283) with
# the _rep_ marker the entry grid keys on. Carrying the letter in the name is
# what keeps a replicate measured alone on its own letter: a positional
# declaration loses the letter before the request is sent, because the form
# drops blank cells rather than sending a hole.

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
    # calcMean/calcSd return the character 'KEEP OLD' when there is nothing to
    # average. Test that before any coercion: as.numeric('KEEP OLD') is NA, so a
    # test made after coercion could not tell the sentinel from a computed NaN.
    if (is.character(value)) return(res)
    if (!is.numeric(value) || length(value) != 1L) return(res)
    # NA is the prelude's "cannot compute", which the portal leaves blank. NaN
    # and Inf are values the portal displays, so they are emitted and the JSON
    # layer decides; a plausibility filter here would disagree with the portal
    # on exactly the rows an operator would come asking about.
    if (is.na(value) && !is.nan(value)) return(res)
    res[[key]] <- value
    res
  }
  # Whether the caller actually sent a usable value, for inputs_used accounting.
  supplied <- function(name) {
    v <- inputs[[name]]
    if (is.null(v) || length(v) == 0L) return(FALSE)
    if (is.list(v)) return(TRUE)
    !is.na(suppressWarnings(v[[1]]))
  }
  # The portal's checkboxes have two states only, so an unrecognised value falls
  # back to the manifest default rather than reaching a branch the portal has not
  # got. The API's enum check means this is never exercised through /calculate.
  mode_of <- function(v) {
    m <- if (is.null(v) || length(v) == 0L) "db" else as.character(v[[1]])
    if (m %in% c("db", "cst")) m else "db"
  }

  # Field data the portal reads from the row (pco2_tool.R:191)
  water_temp_c <- num(inputs$water_temp_c)
  field_bp <- num(inputs$pressure_hpa)
  field_bp_altitude <- num(inputs$pressure_altitude_hpa)

  pool <- riverdata.tools::constants_df(list(
    c_const = num(constants$c_const),
    gas_const_r_atm = num(constants$gas_const_r_atm),
    gas_const_r_mol = num(constants$gas_const_r_mol),
    h_ch4_29815k = num(constants$h_ch4_29815k),
    ch4_in_sa = num(constants$ch4_in_sa),
    lab_temp_avg_degC = num(constants$lab_temp_avg_degC),
    lab_press_avg_atm = num(constants$lab_press_avg_atm),
    vol_sa = num(constants$vol_sa),
    vol_water = num(constants$vol_water)
  ))

  res <- list()

  labTemp <- mode_of(inputs$lab_temp_mode)
  labPa <- mode_of(inputs$lab_pressure_mode)

  lab_temp_c <- num(inputs$lab_temp_c)
  lab_press_hpa <- num(inputs$lab_pressure_hpa)

  rep_letters <- c("A", "B")
  # Portal column base per measurement. The chain below refers to a cell by the
  # short name so the request field name is written down once, here.
  rep_params <- c(co2 = "lab_co2_co2ppm", h2o = "lab_co2_h2o",
                  ch4 = "lab_co2_ch4", d13 = "lab_co2_ico2")
  reps <- setNames(lapply(rep_letters, function(rep) {
    lapply(rep_params, function(base) num(inputs[[paste0(base, "_rep_", rep)]]))
  }), rep_letters)

  per <- list()
  for (rep in rep_letters) {
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
    co2_hs <- calcCO2(co2_df, pool, labTemp, labPa)

    p_df <- riverdata.tools::row_df(setNames(
      list(water_temp_c, field_bp, field_bp_altitude, co2_hs),
      c("WTW_Temp_degC_1", "Field_BP", "Field_BP_altitude",
        paste0("CO2_HS_Um_", rep))
    ))
    pco2 <- calcpCO2(p_df, pool)
    pco2_p1 <- calcpCO2P1(p_df, pool)
    pco2_p2 <- calcpCO2P2(p_df, pool)

    ch4_df <- riverdata.tools::row_df(setNames(
      list(lab_temp_c, lab_press_hpa, water_temp_c, field_bp, field_bp_altitude, ch4_dry),
      c("lab_co2_lab_temp", "lab_co2_lab_press", "WTW_Temp_degC_1",
        "Field_BP", "Field_BP_altitude", paste0("lab_co2_ch4_dry_", rep))
    ))
    ch4_dissolved <- calcCH4(ch4_df, pool, labTemp, labPa)

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
    riverdata.tools::row_df(setNames(
      lapply(rep_letters, function(rep) per[[rep]][[field]]),
      rep_letters
    ))
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

  # Only what this run actually read. The lab-condition entry is read only in
  # 'db' mode; the altitude pressure only when Field_BP fails the 700-1050 band.
  used <- character(0)
  rep_names <- as.vector(outer(rep_params, rep_letters,
                               function(base, rep) paste0(base, "_rep_", rep)))
  for (name in c("water_temp_c", rep_names,
                 "lab_temp_mode", "lab_pressure_mode", "pressure_hpa")) {
    if (supplied(name)) used <- c(used, name)
  }
  if (labTemp == "db" && supplied("lab_temp_c")) used <- c(used, "lab_temp_c")
  if (labPa == "db" && supplied("lab_pressure_hpa")) used <- c(used, "lab_pressure_hpa")
  field_bp_used <- !is.na(field_bp) && field_bp <= 1050 && field_bp >= 700
  if (!field_bp_used && supplied("pressure_altitude_hpa")) {
    used <- c(used, "pressure_altitude_hpa")
  }
  res$inputs_used <- used
  res
}
