# field_data: barometric pressure from altitude, pressure selection, CO2 correction,
# reach depth stats. Orchestration mirrors the portal's field_data_tool.R calculate step:
# calcAlt2BP, then calcCO2corr per min/avg/max column, then calcMean/calcSd on the depths.

# The prelude functions read stations and standard_curves through getRows(pool, ...).
# The pool here is a named list of one-row data frames holding the request's resolved values.
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
  pressure_hpa <- num(inputs$pressure_hpa)

  curve <- curves$std_curve
  if (is.null(curve)) curve <- inputs$std_curve
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
  curve_id <- if (has_curve) 1 else NA_real_

  out <- list()

  alt_bp <- calcAlt2BP(
    data.frame(station = 'site', WTW_Temp_degC_1 = temp, stringsAsFactors = FALSE),
    pool
  )
  if (is.numeric(alt_bp) && length(alt_bp) == 1L && is.finite(alt_bp)) {
    out$Field_BP_altitude <- alt_bp
  }

  # An explicit pressure_hpa rides the portal's Field_BP slot; calcCO2corr owns the
  # 700-1050 in-range check and the fallback to the altitude-derived pressure.
  eff_field_bp <- if (!is.na(pressure_hpa)) pressure_hpa else field_bp
  alt_for_corr <- if (is.numeric(alt_bp) && length(alt_bp) == 1L && is.finite(alt_bp)) {
    alt_bp
  } else {
    NA_real_
  }

  corr <- function(raw) {
    df <- data.frame(
      raw = raw,
      WTW_Temp_degC_1 = temp,
      Field_BP = eff_field_bp,
      Field_BP_altitude = alt_for_corr,
      vaisala_std_curve_id = curve_id
    )
    calcCO2corr(df, pool)
  }
  put_corr <- function(key, raw) {
    v <- corr(raw)
    if (is.numeric(v) && length(v) == 1L && is.finite(v)) out[[key]] <<- v
  }

  co2_min <- num(inputs$raw_co2_min)
  co2_avg <- num(inputs$raw_co2_avg)
  co2_max <- num(inputs$raw_co2_max)
  co2_single <- num(inputs$raw_co2)

  if (!is.na(co2_min) || !is.na(co2_avg) || !is.na(co2_max)) {
    if (!is.na(co2_min)) put_corr('Vaisala_CO2_min_corr', co2_min)
    if (!is.na(co2_avg)) put_corr('Vaisala_CO2_avg_corr', co2_avg)
    if (!is.na(co2_max)) put_corr('Vaisala_CO2_max_corr', co2_max)
  } else if (!is.na(co2_single)) {
    put_corr('Vaisala_CO2_avg_corr', co2_single)
  }

  depths <- inputs$reach_depths
  if (!is.null(depths) && length(depths) > 0L) {
    vals <- vapply(
      as.list(depths),
      function(v) if (is.null(v) || length(v) == 0L) NA_real_ else as.numeric(v),
      numeric(1)
    )
    depth_df <- as.data.frame(
      as.list(stats::setNames(vals, paste0('Reach_depth_rep', seq_along(vals))))
    )
    avg <- calcMean(depth_df)
    stdev <- calcSd(depth_df)
    if (is.numeric(avg) && length(avg) == 1L && is.finite(avg)) {
      out$Reach_depth_avg_cm <- avg
    }
    if (is.numeric(stdev) && length(stdev) == 1L && is.finite(stdev)) {
      out$Reach_depth_sd_cm <- stdev
    }
  }

  out
}
