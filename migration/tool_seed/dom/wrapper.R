tool <- function(inputs, constants, curves) {
  num <- function(x) if (is.null(x) || length(x) == 0) NA_real_ else as.numeric(x)

  res <- list()
  # NA is the portal's "no value": it is what calcSUVA and calcRatio return when they refuse
  # (calculation_functions.R:258, :281), so only NA is omitted. Every other number the portal
  # would display is emitted, including the Inf calcSUVA produces on DOC_avg_ppb = 0 and the
  # NaN it produces on 0 / 0: calcSUVA guards only is.na, unlike calcRatio, which also guards
  # divisor != 0 (calculation_functions.R:253 vs :277). is.na() is TRUE for NaN as well, hence
  # the explicit is.nan() readmission.
  put <- function(key, value) {
    if (is.numeric(value) && length(value) == 1 && (is.nan(value) || !is.na(value))) {
      res[[key]] <<- value
    }
  }

  # Portal: modules/tools_tab/tools/dom_tool.R:153-164
  # data.frame(SUVA = calcSUVA(bind_cols(select(rawDataUpdated(), a254),
  #                                      select(row(), DOC_avg_ppb))),
  #            A_T = calcRatio(select(rawDataUpdated(), A, T)),
  #            C_A = calcRatio(select(rawDataUpdated(), C, A)),
  #            C_M = calcRatio(select(rawDataUpdated(), C, M)),
  #            C_T = calcRatio(select(rawDataUpdated(), C, T)))
  put('SUVA', calcSUVA(data.frame(
    a254 = num(inputs$a254),
    DOC_avg_ppb = num(inputs$doc_avg_ppb)
  )))
  # calcRatio pulls columns positionally: first is dividend, second divisor
  put('A_T', calcRatio(data.frame(A = num(inputs$peak_a), T = num(inputs$peak_t))))
  put('C_A', calcRatio(data.frame(C = num(inputs$peak_c), A = num(inputs$peak_a))))
  put('C_M', calcRatio(data.frame(C = num(inputs$peak_c), M = num(inputs$peak_m))))
  put('C_T', calcRatio(data.frame(C = num(inputs$peak_c), T = num(inputs$peak_t))))

  res
}
