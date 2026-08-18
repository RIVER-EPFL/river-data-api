# Alkalinity raw entry. The portal computes nothing from the Alk_ columns; its one
# calculation fills WTW_pH_1 from Alk_init_pH when missing (calcEquals). Raw values
# echo through so the save path can persist them.

tool <- function(inputs, constants, curves) {
  num <- function(x) {
    if (is.null(x) || length(x) == 0) return(NA_real_)
    v <- suppressWarnings(as.numeric(x[[1]]))
    if (length(v) == 0) NA_real_ else v
  }

  out <- list()
  echo_fields <- c(
    'Alk_meqL', 'Alk_mgL', 'Alk_w_weight_g', 'Alk_dyn_pH',
    'Alk_dyn_trit', 'Alk_temp_degC', 'Alk_init_pH'
  )
  for (field in echo_fields) {
    v <- num(inputs[[field]])
    if (is.finite(v)) out[[field]] <- v
  }

  ph <- calcEquals(data.frame(
    WTW_pH_1 = num(inputs[['WTW_pH_1']]),
    Alk_init_pH = num(inputs[['Alk_init_pH']])
  ))
  if (is.numeric(ph) && length(ph) == 1 && is.finite(ph)) {
    out[['WTW_pH_1']] <- ph
  }

  out
}
