tool <- function(inputs, constants, curves) {
  num <- function(x) if (is.null(x) || length(x) == 0) NA_real_ else as.numeric(x)

  df <- data.frame(
    lab_tss_wgt_samp_filt_dried = num(inputs$wgt_dried_g),
    lab_tss_wgt_filt_prefiltr = num(inputs$wgt_prefilt_g),
    lab_tss_wgt_samp_filt_ashed = num(inputs$wgt_ashed_g),
    lab_tss_vol_filtered = num(inputs$vol_filtered_ml)
  )

  out <- list()

  # 'KEEP OLD' is tested before coercion: as.numeric('KEEP OLD') is NA and would be
  # indistinguishable from a computed NaN. NaN and Inf are values the portal displays.
  emit <- function(x) !identical(x, 'KEEP OLD') && (!is.na(x) || is.nan(x))

  tss <- calcTSS(df)
  if (emit(tss)) {
    out$TSS_dry_weight_mgL <- as.numeric(tss)
  }

  afdm <- calcAFDM(df)
  if (emit(afdm)) {
    out$AFDM_mgL <- as.numeric(afdm)
  }

  out
}
