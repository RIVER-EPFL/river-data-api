# chla_benthic: portal Chl a tool chain (chla_tool.R) per replicate:
# calcMinus -> calcChlaAcid/calcChlaNoAcid -> calcChlaPerM2/calcBenthicAFDM -> calcMean/calcSd.

# In-memory stand-in for the portal's standard_curves lookup used by
# calcChlaAcid/calcChlaNoAcid; pool is a named list of data frames.
getRows <- function(pool, table, ..., columns = NULL) {
  df <- dplyr::filter(pool[[table]], ...)
  if (!is.null(columns)) df <- dplyr::select(df, dplyr::all_of(columns))
  df
}

tool <- function(inputs, constants, curves) {
  num <- function(v) {
    if (is.null(v) || length(v) == 0L) return(NA_real_)
    v <- suppressWarnings(as.numeric(v[[1L]]))
    if (length(v) == 0L || is.na(v)) NA_real_ else v
  }
  keep <- function(x) {
    is.numeric(x) && length(x) == 1L && is.finite(x)
  }
  as_num <- function(x) if (keep(x)) as.numeric(x) else NA_real_

  curve_coefs <- function(curve, slope_in, intercept_in) {
    if (!is.null(curve) && length(curve) > 0L) {
      c(num(curve$slope), num(curve$intercept))
    } else {
      c(num(slope_in), num(intercept_in))
    }
  }
  acid_ab <- curve_coefs(curves$chla_acid, inputs$acid_slope, inputs$acid_intercept)
  noacid_ab <- curve_coefs(curves$chla_noacid, inputs$noacid_slope, inputs$noacid_intercept)

  curve_rows <- data.frame(id = numeric(0), a = numeric(0), b = numeric(0))
  acid_id <- NA_real_
  noacid_id <- NA_real_
  if (!any(is.na(acid_ab))) {
    acid_id <- 1
    curve_rows <- rbind(curve_rows, data.frame(id = 1, a = acid_ab[1], b = acid_ab[2]))
  }
  if (!any(is.na(noacid_ab))) {
    noacid_id <- 2
    curve_rows <- rbind(curve_rows, data.frame(id = 2, a = noacid_ab[1], b = noacid_ab[2]))
  }
  pool <- list(standard_curves = curve_rows)

  reps <- inputs$replicates
  if (is.data.frame(reps)) {
    reps <- lapply(seq_len(nrow(reps)), function(i) {
      lapply(reps, function(col) {
        if (is.matrix(col)) col[i, ] else if (is.list(col)) col[[i]] else col[i]
      })
    })
  }

  rep_rows <- lapply(reps, function(r) {
    fluor1 <- num(r$fluor_before)
    fluor2 <- num(r$fluor_after)
    tot_vol <- num(r$vol_total_ml)
    vol_after <- num(r$vol_after_ml)
    afdm_g <- num(r$afdm_g_filter)
    d <- suppressWarnings(as.numeric(unlist(r$diameters_cm)))
    # The portal reads exactly sizeA/sizeB/sizeC; fewer than 3 dims leaves the
    # per-m2 chain at 'KEEP OLD', extra dims are ignored.
    sizes <- c(d, rep(NA_real_, 3L))[1:3]

    vol_filtered <- as_num(calcMinus(riverdata.tools::row_df(list(
      lab_chla_tot_vol_rep_A = tot_vol,
      lab_chla_vol_after_rep_A = vol_after
    ))))

    chla_acid_ugl <- as_num(calcChlaAcid(riverdata.tools::row_df(list(
      lab_chla_fluor_1_rep_A = fluor1,
      lab_chla_fluor_2_rep_A = fluor2,
      chla_acid_std_curve_id = acid_id
    )), pool))

    chla_noacid_ugl <- as_num(calcChlaNoAcid(riverdata.tools::row_df(list(
      lab_chla_fluor_1_rep_A = fluor1,
      chla_noacid_std_curve_id = noacid_id
    )), pool))

    per_m2_base <- list(
      lab_chla_sizeA_rep_A = sizes[1],
      lab_chla_sizeB_rep_A = sizes[2],
      lab_chla_sizeC_rep_A = sizes[3],
      lab_chla_tot_vol_rep_A = tot_vol,
      lab_chla_vol_filtrated_rep_A = vol_filtered
    )

    chla_acid_ugm2 <- as_num(calcChlaPerM2(riverdata.tools::row_df(
      c(per_m2_base, list(chla_acid_ugL_rep_A = chla_acid_ugl))
    )))
    chla_noacid_ugm2 <- as_num(calcChlaPerM2(riverdata.tools::row_df(
      c(per_m2_base, list(chla_noacid_ugL_rep_A = chla_noacid_ugl))
    )))
    afdm_gm2 <- as_num(calcBenthicAFDM(riverdata.tools::row_df(
      c(per_m2_base, list(afdm_g_filter_rep_A = afdm_g))
    )))

    # convertToUnitPerM2(s, d, vf, vt) is s * vt / (vf * area); unit arguments
    # isolate the prelude's rock area without restating the formula.
    rock_area <- if (!any(is.na(sizes))) 1 / convertToUnitPerM2(1, sizes, 1, 1) else NA_real_

    list(
      vol_filtered_ml = vol_filtered,
      Chla_acid_ugL = chla_acid_ugl,
      Chla_noacid_ugL = chla_noacid_ugl,
      rock_area_m2 = rock_area,
      Chla_acid_ugm2 = chla_acid_ugm2,
      Chla_noacid_ugm2 = chla_noacid_ugm2,
      benthic_AFDM_gm2 = afdm_gm2
    )
  })

  vals <- function(key) {
    as.numeric(vapply(rep_rows, function(x) x[[key]], numeric(1)))
  }
  agg <- function(v, fn) {
    df <- as.data.frame(as.list(setNames(v, paste0("rep_", seq_along(v)))))
    fn(df)
  }

  out <- list()
  put <- function(key, val) {
    if (keep(val)) out[[key]] <<- val
  }

  acid_l <- vals("Chla_acid_ugL")
  noacid_l <- vals("Chla_noacid_ugL")
  acid_m2 <- vals("Chla_acid_ugm2")
  noacid_m2 <- vals("Chla_noacid_ugm2")
  afdm_m2 <- vals("benthic_AFDM_gm2")

  put("Chla_acid_ugL_avg", agg(acid_l, calcMean))
  put("Chla_acid_ugL_sd", agg(acid_l, calcSd))
  put("Chla_noacid_ugL_avg", agg(noacid_l, calcMean))
  put("Chla_noacid_ugL_sd", agg(noacid_l, calcSd))
  put("Chla_acid_avg_ugm2", agg(acid_m2, calcMean))
  put("Chla_acid_sd_ugm2", agg(acid_m2, calcSd))
  put("Chla_noacid_avg_ugm2", agg(noacid_m2, calcMean))
  put("Chla_noacid_sd_ugm2", agg(noacid_m2, calcSd))
  put("benthic_AFDM_avg_gm2", agg(afdm_m2, calcMean))
  put("benthic_AFDM_sd_gm2", agg(afdm_m2, calcSd))

  out$replicates <- lapply(rep_rows, function(r) Filter(keep, r))

  out
}
