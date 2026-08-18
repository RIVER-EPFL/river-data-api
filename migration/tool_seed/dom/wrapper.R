tool <- function(inputs, constants, curves) {
  num <- function(x) if (is.null(x) || length(x) == 0) NA_real_ else as.numeric(x)

  res <- list()
  put <- function(key, value) {
    if (is.numeric(value) && length(value) == 1 && is.finite(value)) res[[key]] <<- value
  }

  put('SUVA', calcSUVA(data.frame(
    a254 = num(inputs$a254),
    DOC_avg_ppb = num(inputs$doc_avg_ppb)
  )))
  # calcRatio pulls columns positionally: first is dividend, second divisor
  put('absorbance_ratio', calcRatio(data.frame(
    n = num(inputs$abs_numerator),
    d = num(inputs$abs_denominator)
  )))
  put('A_T', calcRatio(data.frame(n = num(inputs$peak_a), d = num(inputs$peak_t))))
  put('C_A', calcRatio(data.frame(n = num(inputs$peak_c), d = num(inputs$peak_a))))
  put('C_M', calcRatio(data.frame(n = num(inputs$peak_c), d = num(inputs$peak_m))))
  put('C_T', calcRatio(data.frame(n = num(inputs$peak_c), d = num(inputs$peak_t))))

  res
}
