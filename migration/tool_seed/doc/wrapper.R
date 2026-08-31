# DOC tool wrapper. The replicates arrive as one vector (a blank vial is NA at its own
# position), the standard curve corrects each one, and calcMean/calcSd summarise them for
# display. The stored DOC is the replicates themselves: the database applies the same curve
# per reading and derives the same mean and sd.

tool <- function(inputs, constants, curves) {
  reps <- suppressWarnings(as.numeric(unlist(inputs[['DOC']])))
  if (length(reps) == 0L || all(is.na(reps))) return(list())

  # calcDOC's correction, applied to however many replicates there are.
  curve <- riverdata.tools::curve_df(curves$std_curve)
  if (!is.null(curve)) reps <- reps * curve$a + curve$b

  df <- as.data.frame(as.list(reps))
  avg <- calcMean(df)
  stdev <- calcSd(df)

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
