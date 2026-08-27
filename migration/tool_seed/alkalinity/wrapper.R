# Alkalinity raw entry. The portal computes nothing from the Alk_ columns; its one
# calculation fills WTW_pH_1 from Alk_init_pH when missing (calcEquals). Raw values
# echo through so the save path can persist them.

tool <- function(inputs, constants, curves) {
  num <- function(x) {
    if (is.null(x) || length(x) == 0) return(NA_real_)
    v <- suppressWarnings(as.numeric(x[[1]]))
    if (length(v) == 0) NA_real_ else v
  }

  # Only a genuine NA is a blank cell. NaN and Inf are values the portal displays,
  # so they are emitted rather than filtered.
  emit <- function(x) is.numeric(x) && length(x) == 1L && (!is.na(x) || is.nan(x))

  out <- list()
  echo_fields <- c(
    'Alk_meqL', 'Alk_mgL', 'Alk_w_weight_g', 'Alk_dyn_pH',
    'Alk_dyn_trit', 'Alk_temp_degC', 'Alk_init_pH'
  )
  for (field in echo_fields) {
    v <- num(inputs[[field]])
    if (emit(v)) out[[field]] <- v
  }

  ph <- calcEquals(data.frame(
    WTW_pH_1 = num(inputs[['WTW_pH_1']]),
    Alk_init_pH = num(inputs[['Alk_init_pH']])
  ))
  if (emit(ph)) {
    out[['WTW_pH_1']] <- ph
  }

  out
}
