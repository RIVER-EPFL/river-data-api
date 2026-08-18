# chlorophyll: single acid/no-acid chlorophyll-a curve application.
# Portal: chla_tool.R -> calcChlaAcid / calcChlaNoAcid. The request carries the
# curve coefficients directly (slope/intercept fields), so they are staged as a
# one-row standard_curves table for the prelude functions to resolve.

getRows <- function(pool, table, ..., columns = NULL) {
  rows <- dplyr::filter(pool[[table]], ...)
  if (!is.null(columns)) {
    rows <- dplyr::select(rows, dplyr::all_of(columns))
  }
  rows
}

tool <- function(inputs, constants, curves) {
  num_or_na <- function(v) {
    if (is.null(v) || length(v) == 0L) NA_real_ else as.numeric(v)
  }

  slope <- num_or_na(inputs$slope)
  intercept <- num_or_na(inputs$intercept)
  curve_id <- if (is.na(slope) || is.na(intercept)) NA_real_ else 1
  pool <- list(standard_curves = data.frame(id = 1, a = slope, b = intercept))

  method <- as.character(inputs$method)
  results <- list()

  if (identical(method, "acid")) {
    df <- riverdata.tools::row_df(list(
      lab_chla_fluor_1_rep = inputs$fluorescence_before,
      lab_chla_fluor_2_rep = inputs$fluorescence_after,
      chla_acid_std_curve_id = curve_id
    ))
    val <- calcChlaAcid(df, pool)
    if (is.numeric(val) && length(val) == 1L && is.finite(val)) {
      results$Chla_acid_ugL_avg <- val
    }
  } else if (identical(method, "no_acid")) {
    df <- riverdata.tools::row_df(list(
      lab_chla_fluor_1_rep = inputs$fluorescence_before,
      chla_noacid_std_curve_id = curve_id
    ))
    val <- calcChlaNoAcid(df, pool)
    if (is.numeric(val) && length(val) == 1L && is.finite(val)) {
      results$Chla_noacid_ugL_avg <- val
    }
  } else {
    stop("method must be 'acid' or 'no_acid'")
  }

  results
}
