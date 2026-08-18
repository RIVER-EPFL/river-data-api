tool <- function(inputs, constants, curves) {
  num <- function(x) if (is.null(x) || length(x) == 0) NA_real_ else as.numeric(x)

  df <- data.frame(
    lab_tss_wgt_samp_filt_dried = num(inputs$wgt_dried_g),
    lab_tss_wgt_filt_prefiltr = num(inputs$wgt_prefilt_g),
    lab_tss_wgt_samp_filt_ashed = num(inputs$wgt_ashed_g),
    lab_tss_vol_filtered = num(inputs$vol_filtered_ml)
  )

  out <- list()

  tss <- calcTSS(df)
  if (!identical(tss, 'KEEP OLD') && is.finite(tss)) {
    out$TSS_dry_weight_mgL <- tss
  }

  afdm <- calcAFDM(df)
  if (!identical(afdm, 'KEEP OLD') && is.finite(afdm)) {
    out$AFDM_mgL <- afdm
  }

  out
}
