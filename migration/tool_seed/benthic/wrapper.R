tool <- function(inputs, constants, curves) {
  num_or_na <- function(x) {
    if (is.null(x) || length(x) == 0) return(NA_real_)
    as.numeric(x)[1]
  }

  d <- vapply(
    as.list(inputs$diameters_cm),
    function(x) if (is.null(x)) NA_real_ else as.numeric(x),
    numeric(1)
  )
  vf <- num_or_na(inputs$volume_filtered_ml)
  vt <- num_or_na(inputs$total_volume_ml)
  afdm <- num_or_na(inputs$afdm_g_filter)
  chla <- num_or_na(inputs$chla_ug_l)

  out <- list()

  # convertToUnitPerM2(1, d, 1, 1) is 1 / area, so the area comes from the
  # prelude formula rather than a second copy of it
  if (length(d) >= 2) {
    area <- tryCatch(1 / convertToUnitPerM2(1, d, 1, 1), error = function(e) NaN)
    if (is.finite(area)) out$rock_surface_area_m2 <- area
  }

  # One-row frame in the portal's column shape (single replicate A);
  # calcBenthicAFDM / calcChlaPerM2 use exactly sizeA/B/C
  base_df <- data.frame(
    lab_chla_sizeA_rep_A = if (length(d) >= 1) d[1] else NA_real_,
    lab_chla_sizeB_rep_A = if (length(d) >= 2) d[2] else NA_real_,
    lab_chla_sizeC_rep_A = if (length(d) >= 3) d[3] else NA_real_,
    lab_chla_tot_vol_rep_A = vt,
    lab_chla_vol_filtrated_rep_A = vf
  )

  emit <- function(res) {
    if (identical(res, "KEEP OLD")) return(NULL)
    res <- suppressWarnings(as.numeric(res))
    if (length(res) == 1 && is.finite(res)) res else NULL
  }

  if (!is.na(afdm)) {
    df <- base_df
    df$afdm_g_filter_rep_A <- afdm
    v <- emit(calcBenthicAFDM(df))
    if (!is.null(v)) out$benthic_AFDM_avg_gm2 <- v
  }

  if (!is.na(chla)) {
    df <- base_df
    df$chla_acid_ugL_rep_A <- chla
    v <- emit(calcChlaPerM2(df))
    if (!is.null(v)) out$Chla_avg_ugm2 <- v
  }

  out
}
