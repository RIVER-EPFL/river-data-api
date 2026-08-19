# chlorophyll: the portal Chl a tool tab (modules/tools_tab/tools/chla_tool.R), 1:1.
#
# The portal's calculate observer (chla_tool.R:274-496) loops replicates A..E and, for every
# replicate, computes BOTH the acid and the no-acid variant unconditionally, plus the two
# calcMinus-derived columns (volume filtered, AFDM on filter), the two per-m2 chla conversions
# and the per-m2 benthic AFDM. It then loops five parameter families and writes an avg and an
# sd column for each.
#
# 'KEEP OLD' is the portal's "leave the stored cell as it was" sentinel (it reads the value back
# out of row()). This tool is stateless, so there is no old value to carry forward: a 'KEEP OLD'
# becomes NA and the key is omitted from the result, matching the tools-tab behaviour of
# dropping NAs.

# In-memory stand-in for the portal's standard_curves lookup used by
# calcChlaAcid/calcChlaNoAcid; pool is a named list of data frames.
getRows <- function(pool, table, ..., columns = NULL) {
  rows <- dplyr::filter(pool[[table]], ...)
  if (!is.null(columns)) {
    rows <- dplyr::select(rows, dplyr::all_of(columns))
  }
  rows
}

tool <- function(inputs, constants, curves) {
  num <- function(v) {
    if (is.null(v) || length(v) == 0L) return(NA_real_)
    v <- suppressWarnings(as.numeric(v[[1L]]))
    if (length(v) == 0L) NA_real_ else v
  }
  # A cell the portal leaves at its stored value ('KEEP OLD') or writes NA into has no number to
  # report, so its key is omitted. NaN and Inf are numbers the portal displays, so they are
  # emitted: JSON cannot encode either, so they serialise to null and the API drops them, which
  # is absence arriving by a different route rather than a plausibility filter here.
  emit <- function(x) is.numeric(x) && length(x) == 1L && (!is.na(x) || is.nan(x))
  # The portal's ifelse(x != 'KEEP OLD', x, pull(row(), col)) with no row() to fall back on.
  # Only the 'KEEP OLD' sentinel becomes NA: the portal's ifelse compares a numeric against the
  # string, so Inf/NaN test TRUE and are stored and propagated unchanged into the downstream
  # conversions and the avg/sd. The sentinel is tested before any numeric coercion, since
  # as.numeric('KEEP OLD') is NA and would then be indistinguishable from a computed NaN.
  keep_old <- function(x) {
    if (is.character(x) || length(x) != 1L) return(NA_real_)
    as.numeric(x)
  }

  # Standard curves: the portal's two selectInputs (chla_tool.R:40-61) resolve to
  # chla_acid_std_curve_id / chla_noacid_std_curve_id, or NA when left on 'Select a date...'.
  curve_rows <- data.frame(id = numeric(0), a = numeric(0), b = numeric(0))
  acid_id <- NA_real_
  noacid_id <- NA_real_
  acid_curve <- riverdata.tools::curve_df(curves$chla_acid)
  noacid_curve <- riverdata.tools::curve_df(curves$chla_noacid)
  if (!is.null(acid_curve) && !any(is.na(c(acid_curve$a, acid_curve$b)))) {
    acid_id <- 1
    curve_rows <- rbind(curve_rows, data.frame(id = 1, a = acid_curve$a, b = acid_curve$b))
  }
  if (!is.null(noacid_curve) && !any(is.na(c(noacid_curve$a, noacid_curve$b)))) {
    noacid_id <- 2
    curve_rows <- rbind(curve_rows, data.frame(id = 2, a = noacid_curve$a, b = noacid_curve$b))
  }
  pool <- list(standard_curves = curve_rows)

  raw <- function(base, rep) num(inputs[[paste0(base, rep)]])

  ## Per-replicate calculations (chla_tool.R:276-438) ############################
  calculations_chla <- NULL

  for (rep in c('A', 'B', 'C', 'D', 'E')) {
    colNames <- paste0(
      c('lab_chla_vol_filtrated_rep_',
        'chla_acid_ugL_rep_',
        'chla_acid_ugm2_rep_',
        'chla_noacid_ugL_rep_',
        'chla_noacid_ugm2_rep_',
        'afdm_g_filter_rep_',
        'afdm_gm2_rep_'),
      rep
    )

    sizeA <- raw('lab_chla_sizeA_rep_', rep)
    sizeB <- raw('lab_chla_sizeB_rep_', rep)
    sizeC <- raw('lab_chla_sizeC_rep_', rep)
    tot_vol <- raw('lab_chla_tot_vol_rep_', rep)
    vol_after <- raw('lab_chla_vol_after_rep_', rep)
    fluor_1 <- raw('lab_chla_fluor_1_rep_', rep)
    fluor_2 <- raw('lab_chla_fluor_2_rep_', rep)
    wgt_1 <- raw('lab_chla_wgt_1_rep_', rep)
    wgt_2 <- raw('lab_chla_wgt_2_rep_', rep)

    # chla_tool.R:296-306
    lab_chla_vol_filtrated_rep <- calcMinus(riverdata.tools::row_df(setNames(
      list(tot_vol, vol_after),
      paste0(c('lab_chla_tot_vol_rep_', 'lab_chla_vol_after_rep_'), rep)
    )))

    # chla_tool.R:308-330
    chla_acid_ugL_rep <- calcChlaAcid(
      riverdata.tools::row_df(setNames(
        list(fluor_1, fluor_2, acid_id),
        c(paste0(c('lab_chla_fluor_1_rep_', 'lab_chla_fluor_2_rep_'), rep),
          'chla_acid_std_curve_id')
      )),
      pool
    )
    chla_acid_ugL_rep <- keep_old(chla_acid_ugL_rep)

    # chla_tool.R:332-351
    chla_noacid_ugL_rep <- calcChlaNoAcid(
      riverdata.tools::row_df(setNames(
        list(fluor_1, noacid_id),
        c(paste0('lab_chla_fluor_1_rep_', rep), 'chla_noacid_std_curve_id')
      )),
      pool
    )
    chla_noacid_ugL_rep <- keep_old(chla_noacid_ugL_rep)

    # chla_tool.R:353-363
    afdm_g_filter_rep <- calcMinus(riverdata.tools::row_df(setNames(
      list(wgt_1, wgt_2),
      paste0(c('lab_chla_wgt_1_rep_', 'lab_chla_wgt_2_rep_'), rep)
    )))

    # chla_tool.R:365-378
    perM2Cols <- riverdata.tools::row_df(setNames(
      list(sizeA, sizeB, sizeC, tot_vol, lab_chla_vol_filtrated_rep),
      c(paste0(c('lab_chla_sizeA_rep_', 'lab_chla_sizeB_rep_', 'lab_chla_sizeC_rep_',
                 'lab_chla_tot_vol_rep_'), rep),
        colNames[1])
    ))
    with_col <- function(name, value) {
      df <- perM2Cols
      df[[name]] <- value
      df
    }

    # chla_tool.R:380-401
    chla_acid_ugm2_rep <- calcChlaPerM2(with_col(colNames[2], chla_acid_ugL_rep))
    chla_acid_ugm2_rep <- keep_old(chla_acid_ugm2_rep)

    chla_noacid_ugm2_rep <- calcChlaPerM2(with_col(colNames[4], chla_noacid_ugL_rep))
    chla_noacid_ugm2_rep <- keep_old(chla_noacid_ugm2_rep)

    # chla_tool.R:403-413
    afdm_gm2_rep <- calcBenthicAFDM(with_col(colNames[6], afdm_g_filter_rep))
    afdm_gm2_rep <- keep_old(afdm_gm2_rep)

    # chla_tool.R:415-437
    newCols <- riverdata.tools::row_df(setNames(
      list(lab_chla_vol_filtrated_rep,
           chla_acid_ugL_rep,
           chla_acid_ugm2_rep,
           chla_noacid_ugL_rep,
           chla_noacid_ugm2_rep,
           afdm_g_filter_rep,
           afdm_gm2_rep),
      colNames
    ))

    if (is.null(calculations_chla)) {
      calculations_chla <- newCols
    } else {
      calculations_chla <- dplyr::bind_cols(calculations_chla, newCols)
    }
  }

  ## Avg and sd per parameter family (chla_tool.R:440-492) #######################
  calculations_avgSd <- list()

  for (param in c('_acid_ugm2', '_noacid_ugm2', '_acid_ugL', '_noacid_ugL', 'afdm_gm2')) {
    df <- calculations_chla %>% dplyr::select(dplyr::matches(param))

    newMean <- calcMean(df)
    newSd <- calcSd(df)
    if (param == 'afdm_gm2') {
      meanCol <- 'benthic_AFDM_avg_gm2'
      sdCol <- 'benthic_AFDM_sd_gm2'
    } else {
      if (grepl('ugL', param)) {
        meanCol <- paste0('Chla', param, '_avg')
        sdCol <- paste0('Chla', param, '_sd')
      } else {
        paramSplitted <- unlist(strsplit(param, '_'))[-1]
        meanCol <- paste('Chla', paramSplitted[1], 'avg', paramSplitted[2], sep = '_')
        sdCol <- paste('Chla', paramSplitted[1], 'sd', paramSplitted[2], sep = '_')
      }
    }

    calculations_avgSd[[meanCol]] <- keep_old(newMean)
    calculations_avgSd[[sdCol]] <- keep_old(newSd)
  }

  ## Result #####################################################################
  # The tools tab renders the two calculated tables; a cell the portal would have left at its
  # stored value has no stateless equivalent, so its key is omitted.
  out <- list()
  for (nm in names(calculations_avgSd)) {
    if (emit(calculations_avgSd[[nm]])) out[[nm]] <- calculations_avgSd[[nm]]
  }
  for (nm in colnames(calculations_chla)) {
    v <- calculations_chla[[nm]]
    if (emit(v)) out[[nm]] <- v
  }

  out
}
