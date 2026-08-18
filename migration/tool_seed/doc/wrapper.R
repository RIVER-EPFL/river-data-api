# DOC tool wrapper. Mirrors the portal doc_tool.R orchestration: three DOC_rep columns
# plus doc_std_curve_id feed calcDOCavg/calcDOCsd; the standard curve is served to
# calcDOC through a local pool shim.

getRows <- function(pool, table, ...) {
  pool[[table]]
}

tool <- function(inputs, constants, curves) {
  reps <- inputs$replicates
  vals <- vapply(seq_len(3), function(i) {
    v <- if (i <= length(reps)) reps[[i]] else NULL
    if (is.null(v) || length(v) == 0L || is.na(v)) NA_real_ else as.numeric(v)
  }, numeric(1))

  curve <- riverdata.tools::curve_df(curves$std_curve)
  pool <- list(standard_curves = curve)

  df <- data.frame(
    DOC_rep_1 = vals[1],
    DOC_rep_2 = vals[2],
    DOC_rep_3 = vals[3],
    doc_std_curve_id = if (is.null(curve)) NA_real_ else 1
  )

  avg <- calcDOCavg(df, pool)
  stdev <- calcDOCsd(df, pool)

  out <- list()
  if (is.numeric(avg) && length(avg) == 1L && is.finite(avg)) out$DOC_avg_ppb <- avg
  if (is.numeric(stdev) && length(stdev) == 1L && is.finite(stdev)) out$DOC_sd_ppb <- stdev
  out
}
