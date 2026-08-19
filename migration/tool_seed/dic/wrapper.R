# DIC tool: portal modules/tools_tab/tools/dic_tool.R orchestration over
# calcDIC / calcd13DIC / calcMean / calcSd.
#
# dic_tool.R:187-191 -- the 'Use lab temp constant' checkbox selects labTemp
# 'cst' (constants table) or 'db' (the lab constant table cell). The functions'
# 'default' mode is never reached from the portal, so it is not offered here.
# dic_tool.R:194-227 -- replicates A and B are looped unconditionally for both
# DIC and d13C_DIC; a missing replicate simply yields NA.
# dic_tool.R:235-272 -- avg/std are calcMean/calcSd over the replicate columns.
# 'KEEP OLD' carries the stored value forward in the portal; this tool is
# stateless, so the key is omitted instead.

getRows <- function(pool, table, ..., columns = NULL) {
  df <- pool[[table]]
  df <- dplyr::filter(df, ...)
  if (!is.null(columns)) df <- dplyr::select(df, dplyr::any_of(columns))
  df
}

tool <- function(inputs, constants, curves) {
  num <- function(v) if (is.null(v) || length(v) == 0L) NA_real_ else as.numeric(v)

  # Every constant comes from the constants table (calcDIC:308-311, 342-345).
  pool <- list(constants = riverdata.tools::constants_df(list(
    h_co2_29815k = num(constants[["h_co2_29815k"]]),
    gas_const_r_mol = num(constants[["gas_const_r_mol"]]),
    vial_volume = num(constants[["vial_volume"]]),
    h3po4_added = num(constants[["h3po4_added"]]),
    lab_temp_avg_degC = num(constants[["lab_temp_avg_degC"]]),
    lab_press_avg_atm = num(constants[["lab_press_avg_atm"]])
  )))

  # dic_tool.R:187-191. The checkbox can only produce 'cst' or 'db', so anything
  # else is normalised to the unchecked state rather than reaching the functions'
  # 'default' branch (calculation_functions.R:323-330), which the portal cannot run.
  labTemp <- inputs[["lab_temp_mode"]]
  if (is.null(labTemp) || length(labTemp) == 0L || is.na(labTemp)) labTemp <- "db"
  labTemp <- as.character(labTemp)
  if (!labTemp %in% c("cst", "db")) labTemp <- "db"

  # dic_tool.R:115-120, 200-203: the lab constant row is shared by both
  # replicates and bound to each replicate's raw columns.
  lab_temp <- num(inputs[["lab_temp_c"]])

  # The replicate letter is carried by the field name, so a cell left blank
  # cannot shift the remaining cells onto another letter.
  # dic_tool.R:200-203 selects the raw columns ending in the replicate letter;
  # here the same selection is a lookup of '{portal column}_rep_{letter}'.
  rawCols <- c("lab_dic_acid_sample_wght", "lab_dic_acid_wght",
               "lab_dic_vol_overpressure", "lab_dic_SA_added",
               "lab_dic_co2_dry", "lab_dic_delta_13co2")

  rep_df <- function(rep) {
    cells <- lapply(rawCols, function(col) num(inputs[[paste0(col, "_rep_", rep)]]))
    names(cells) <- rawCols
    riverdata.tools::row_df(c(list(lab_dic_air_temp = lab_temp), cells))
  }

  out <- list()
  # calcDIC:358 / calcd13DIC:440 guard only is.na and divisor != 0 and otherwise
  # return the quotient, so NA is the only state the portal has no number for.
  # NaN and Inf are written to the table like any other value, and is.na() is TRUE
  # for NaN, so the test has to let NaN through explicitly.
  put <- function(key, v) {
    if (is.numeric(v) && length(v) == 1L && (is.nan(v) || !is.na(v))) out[[key]] <<- v
  }
  # calcMean/calcSd return the character 'KEEP OLD' in place of a number
  # (calculation_functions.R:40, :55); the sentinel has to be tested before any
  # coercion, since as.numeric('KEEP OLD') is NA and indistinguishable from NaN.
  put_stat <- function(key, v) {
    if (!identical(as.character(v), "KEEP OLD")) put(key, as.numeric(v))
  }

  # dic_tool.R:194-227
  dic <- list()
  for (param in c("DIC", "d13C_DIC")) {
    for (rep in c("A", "B")) {
      dicCol <- paste0(param, "_", rep)

      inputDf <- rep_df(rep)

      dic[[dicCol]] <- as.numeric(ifelse(
        param == "DIC",
        calcDIC(inputDf, pool, labTemp),
        calcd13DIC(inputDf, pool, labTemp)
      ))

      put(dicCol, dic[[dicCol]])
    }
  }

  # dic_tool.R:235-272
  for (param in c("DIC", "d13C_DIC")) {
    reps <- riverdata.tools::row_df(dic[paste0(param, "_", c("A", "B"))])

    newMean <- calcMean(reps)
    newSd <- calcSd(reps)

    # 'KEEP OLD' would take the stored value from row(); stateless here.
    put_stat(paste0(param, "_avg"), newMean)
    put_stat(paste0(param, "_std"), newSd)
  }

  # calcDIC:316-322 / calcd13DIC:400-406 pull lab_dic_air_temp only in 'db' mode.
  # In 'cst' mode the column must exist for the allColumns check but its value is
  # never read, so the entered cell is not a consumed input. 'default' is
  # normalised away above, so no branch that reads the cell is unaccounted for.
  used <- c("lab_temp_mode",
            paste0(rep(rawCols, each = 2L), "_rep_", c("A", "B")))
  if (labTemp == "db") used <- c(used, "lab_temp_c")
  out$inputs_used <- used

  out
}
