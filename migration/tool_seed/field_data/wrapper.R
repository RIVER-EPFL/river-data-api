# field_data: reach-depth statistics, barometric pressure from site elevation, and the three
# Vaisala CO2 corrections. Control flow mirrors field_data_tool.R:287-338 exactly: calcMean /
# calcSd on the Reach_depth_rep_* table, then calcAlt2BP, then calcCO2corr once per
# Vaisala_CO2 column (min, avg, max) against the same 5-column frame the portal builds.

# The prelude functions read `stations` and `standard_curves` through getRows(pool, ...).
# The pool here is a named list of one-row data frames holding the request's resolved values:
# the station row carries the elevation the API resolved from the site record, matching
# calculation_functions.R:93 (getRows(pool, 'stations', name == station, ...) %>% pull(elevation)).
getRows <- function(pool, table, ..., columns = NULL) {
  pool[[table]]
}

tool <- function(inputs, constants, curves) {
  num <- function(x) {
    if (is.null(x) || length(x) == 0L) NA_real_ else as.numeric(x[[1]])
  }

  elevation <- num(inputs$elevation_m)
  temp <- num(inputs$temp_c)
  field_bp <- num(inputs$field_bp)

  curve <- curves$std_curve
  has_curve <- !is.null(curve) && !is.null(curve$slope) && !is.null(curve$intercept)

  pool <- list(
    stations = data.frame(
      name = 'site', order = 1, elevation = elevation, stringsAsFactors = FALSE
    ),
    standard_curves = if (has_curve) {
      data.frame(id = 1, a = as.numeric(curve$slope), b = as.numeric(curve$intercept))
    } else {
      data.frame(id = numeric(0), a = numeric(0), b = numeric(0))
    }
  )
  # stdCurveIds() is NA when the 'Vaisala std curve corr?' checkbox is off
  # (field_data_tool.R:157-172); calcCO2corr then skips the a/b correction.
  curve_id <- if (has_curve) 1 else NA_real_

  # The portal writes a cell only when the calculation produced a value: calcAlt2BP and
  # calcCO2corr return NA when they cannot compute, and calcMean/calcSd return the string
  # 'KEEP OLD'. NaN and Inf are values the portal displays, so they are emitted rather than
  # filtered; NA is the one case that means "nothing was written".
  # as.numeric('KEEP OLD') is NA, so the sentinel is tested before any coercion.
  keep_old <- function(v) is.character(v)
  blank <- function(v) is.na(v) && !is.nan(v)

  out <- list()

  # --- Reach depth avg / sd (field_data_tool.R:289-304) ------------------------------
  # The portal's reach-depth table is the fixed Reach_depth_rep_1..10 column family
  # (tool_field_info.html:17); unfilled replicates are NA and calcMean/calcSd drop them.
  # Each replicate is read by its own portal column name, so an unfilled cell leaves a hole
  # where it was measured instead of shifting the cells after it up one slot.
  # `[[` rather than `$`: `$` partial-matches on lists, so inputs$Reach_depth_rep_1 would read
  # replicate 10 whenever replicate 1 was not filled in.
  depth_df <- data.frame(
    Reach_depth_rep_1 = num(inputs[['Reach_depth_rep_1']]),
    Reach_depth_rep_2 = num(inputs[['Reach_depth_rep_2']]),
    Reach_depth_rep_3 = num(inputs[['Reach_depth_rep_3']]),
    Reach_depth_rep_4 = num(inputs[['Reach_depth_rep_4']]),
    Reach_depth_rep_5 = num(inputs[['Reach_depth_rep_5']]),
    Reach_depth_rep_6 = num(inputs[['Reach_depth_rep_6']]),
    Reach_depth_rep_7 = num(inputs[['Reach_depth_rep_7']]),
    Reach_depth_rep_8 = num(inputs[['Reach_depth_rep_8']]),
    Reach_depth_rep_9 = num(inputs[['Reach_depth_rep_9']]),
    Reach_depth_rep_10 = num(inputs[['Reach_depth_rep_10']])
  )
  new_depth_mean <- calcMean(depth_df)
  new_depth_sd <- calcSd(depth_df)
  # The portal's 'KEEP OLD' carry-forward from row() is not emulated: these tools are
  # stateless, so an uncomputable value is omitted instead.
  if (!keep_old(new_depth_mean)) out$Reach_depth_avg_cm <- as.numeric(new_depth_mean)
  if (!keep_old(new_depth_sd)) out$Reach_depth_sd_cm <- as.numeric(new_depth_sd)

  # --- Field_BP_altitude (field_data_tool.R:307-315) ---------------------------------
  bp_altitude <- calcAlt2BP(
    data.frame(station = 'site', WTW_Temp_degC_1 = temp, stringsAsFactors = FALSE),
    pool
  )
  if (!blank(bp_altitude)) out$Field_BP_altitude <- bp_altitude

  # --- Vaisala CO2 corrections (field_data_tool.R:318-334) ---------------------------
  # co2 <- bind_cols(Vaisala_CO2_*, WTW_Temp_degC_1, Field_BP, Field_BP_altitude,
  #                  vaisala_std_curve_id); each call drops the other two CO2 columns so
  # calcCO2corr sees exactly 5 columns with the raw CO2 first.
  corr <- function(raw) {
    calcCO2corr(
      data.frame(
        raw = raw,
        WTW_Temp_degC_1 = temp,
        Field_BP = field_bp,
        Field_BP_altitude = bp_altitude,
        vaisala_std_curve_id = curve_id
      ),
      pool
    )
  }

  min_corr <- corr(num(inputs$raw_co2_min))
  avg_corr <- corr(num(inputs$raw_co2_avg))
  max_corr <- corr(num(inputs$raw_co2_max))

  if (!blank(min_corr)) out$Vaisala_CO2_min_corr <- min_corr
  if (!blank(avg_corr)) out$Vaisala_CO2_avg_corr <- avg_corr
  if (!blank(max_corr)) out$Vaisala_CO2_max_corr <- max_corr

  out
}
