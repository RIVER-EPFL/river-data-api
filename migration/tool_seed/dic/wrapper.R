# DIC tool: portal dic_tool.R orchestration over calcDIC / calcd13DIC.

getRows <- function(pool, table, ..., columns = NULL) {
  df <- pool[[table]]
  df <- dplyr::filter(df, ...)
  if (!is.null(columns)) df <- dplyr::select(df, dplyr::any_of(columns))
  df
}

tool <- function(inputs, constants, curves) {
  num <- function(v) if (is.null(v) || length(v) == 0L) NA_real_ else as.numeric(v)

  # Request value wins over the constants table, mirroring the Rust tool.
  override <- function(name) {
    v <- num(inputs[[name]])
    if (is.na(v)) num(constants[[name]]) else v
  }

  pool <- list(constants = riverdata.tools::constants_df(list(
    h_co2_29815k = override("h_co2_29815k"),
    gas_const_r_mol = override("gas_const_r_mol"),
    vial_volume = override("vial_volume"),
    h3po4_added = override("h3po4_added"),
    lab_temp_avg_degC = num(constants[["lab_temp_avg_degC"]])
  )))

  lab_temp <- num(inputs[["lab_temp_c"]])

  # calcDIC/calcd13DIC 'default' mode falls back to lab_temp_avg_degC when
  # lab_dic_air_temp is NA, the portal's cst behavior.
  rep_df <- function(r) {
    riverdata.tools::row_df(list(
      lab_dic_air_temp = lab_temp,
      lab_dic_acid_sample_wght = num(r[["acid_sample_weight_g"]]),
      lab_dic_acid_wght = num(r[["acid_weight_g"]]),
      lab_dic_vol_overpressure = num(r[["vol_overpressure_ml"]]),
      lab_dic_SA_added = num(r[["sa_added_ml"]]),
      lab_dic_co2_dry = num(r[["co2_dry_ppm"]]),
      lab_dic_delta_13co2 = num(r[["d13co2_permil"]])
    ))
  }

  out <- list()
  put <- function(key, v) {
    if (is.numeric(v) && length(v) == 1L && is.finite(v)) out[[key]] <<- v
  }

  df_a <- rep_df(inputs)
  dic_a <- calcDIC(df_a, pool)
  d13c_a <- calcd13DIC(df_a, pool)

  rep_b <- inputs[["replicate_b"]]
  if (!is.null(rep_b) && length(rep_b) > 0L) {
    df_b <- rep_df(rep_b)
    dic_b <- calcDIC(df_b, pool)
    d13c_b <- calcd13DIC(df_b, pool)

    dic_reps <- riverdata.tools::row_df(list(DIC_A = dic_a, DIC_B = dic_b))
    d13c_reps <- riverdata.tools::row_df(list(d13C_DIC_A = d13c_a, d13C_DIC_B = d13c_b))

    put("DIC_A", dic_a)
    put("DIC_B", dic_b)
    put("DIC_avg", calcMean(dic_reps))
    put("DIC_std", calcSd(dic_reps))
    put("d13C_DIC_A", d13c_a)
    put("d13C_DIC_B", d13c_b)
    put("d13C_DIC_avg", calcMean(d13c_reps))
    put("d13C_DIC_std", calcSd(d13c_reps))
  } else {
    put("DIC_avg", dic_a)
    put("d13C_DIC_avg", d13c_a)
  }

  out
}
