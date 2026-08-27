# DOC tool wrapper. Mirrors the portal doc_tool.R orchestration: three DOC_rep columns
# plus doc_std_curve_id feed calcDOCavg/calcDOCsd; the standard curve is served to
# calcDOC through a local pool shim.

getRows <- function(pool, table, ...) {
  pool[[table]]
}

tool <- function(inputs, constants, curves) {
  # Each replicate is read by the portal's own column name, so a replicate entered alone stays
  # on its own number instead of shifting to the first free slot.
  num <- function(key) {
    v <- inputs[[key]]
    if (is.null(v) || length(v) == 0L) return(NA_real_)
    v <- suppressWarnings(as.numeric(v[[1L]]))
    if (length(v) == 0L) NA_real_ else v
  }

  curve <- riverdata.tools::curve_df(curves$std_curve)
  pool <- list(standard_curves = curve)

  df <- data.frame(
    DOC_rep_1 = num('DOC_rep_1'),
    DOC_rep_2 = num('DOC_rep_2'),
    DOC_rep_3 = num('DOC_rep_3'),
    doc_std_curve_id = if (is.null(curve)) NA_real_ else 1
  )

  avg <- calcDOCavg(df, pool)
  stdev <- calcDOCsd(df, pool)

  out <- list()
  # calcMean/calcSd return 'KEEP OLD' to leave the portal cell unchanged; this tool is
  # stateless, so the key is omitted instead. The sentinel is tested before coercion because
  # as.numeric('KEEP OLD') is NA and would be indistinguishable from a computed NaN. Nothing
  # else is filtered: NaN and Inf are values the portal displays, so they are emitted.
  emit <- function(key, value) {
    if (identical(value, 'KEEP OLD')) return(invisible(NULL))
    if (is.numeric(value) && length(value) == 1L && (is.nan(value) || !is.na(value))) {
      out[[key]] <<- value
    }
  }
  emit('DOC_avg_ppb', avg)
  emit('DOC_sd_ppb', stdev)
  out
}
