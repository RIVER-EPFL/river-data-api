# benthic: the per-m2 conversion half of the portal Chl a tool tab
# (modules/tools_tab/tools/chla_tool.R, lines 279-496). Rock dimensions and
# volumes convert already-measured chlorophyll (ug/L, acid and no-acid) and
# AFDM (g per filter) to a per-m2 areal load for replicates A-E, then the
# portal avg/sd loop summarises them.
#
# The portal's 'KEEP OLD' fallback (pull the stored DB value when a calculation
# cannot run) has no analogue here: this tool is stateless, so KEEP OLD becomes
# NA and the key is omitted.

tool <- function(inputs, constants, curves) {
  reps_letters <- c('A', 'B', 'C', 'D', 'E')

  # Every cell is its own param, named as the portal names the column, so the
  # replicate letter travels in the field name. A blank cell is simply an
  # absent name and cannot slide onto another letter.
  num <- function(name) {
    v <- inputs[[name]]
    if (is.null(v) || length(v) == 0L) return(NA_real_)
    v <- suppressWarnings(as.numeric(v[[1L]]))
    if (length(v) == 0L || is.na(v)) NA_real_ else v
  }
  # calcChlaPerM2 / calcBenthicAFDM / calcMean / calcSd return either a number or
  # the string 'KEEP OLD'. as.numeric('KEEP OLD') is NA, which no later test can
  # tell from a real NaN, so the sentinel is checked before coercion. The
  # portal's `ifelse(x != 'KEEP OLD', x, old)` coerces a numeric x to a string,
  # so Inf compares unequal and is CARRIED as Inf into the mean and sd beside it.
  as_num <- function(x) {
    if (identical(x, 'KEEP OLD') || length(x) != 1L) return(NA_real_)
    suppressWarnings(as.numeric(x))
  }

  out <- structure(list(), names = character(0))
  # NaN and Inf are values the portal displays, so they are emitted rather than
  # filtered out; both serialise to JSON null and the API drops nulls, so they
  # reach the caller as an absent key. A key is omitted only for a genuine NA,
  # which is what 'KEEP OLD' and a blank input become.
  put <- function(key, val) {
    if (is.numeric(val) && length(val) == 1L && (is.nan(val) || !is.na(val))) {
      out[[key]] <<- val
    }
  }

  # Portal chla_tool.R:279-438 - one pass per replicate letter.
  calculations_chla <- NULL
  for (rep in reps_letters) {
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

    # convertToUnitPerM2 is a three-axis formula and both calcBenthicAFDM and
    # calcChlaPerM2 read exactly sizeA/sizeB/sizeC, so a letter missing any one
    # of the three axes leaves every per-m2 output for that letter at KEEP OLD.
    sizeCols <- paste0(
      c('lab_chla_sizeA_rep_', 'lab_chla_sizeB_rep_', 'lab_chla_sizeC_rep_'),
      rep
    )
    tot_vol <- num(paste0('lab_chla_tot_vol_rep_', rep))
    vol_filtrated <- num(colNames[1])
    chla_acid_ugL_rep <- num(colNames[2])
    chla_noacid_ugL_rep <- num(colNames[4])
    afdm_g_filter_rep <- num(colNames[6])

    perM2Cols <- riverdata.tools::row_df(setNames(
      list(num(sizeCols[1]), num(sizeCols[2]), num(sizeCols[3]), tot_vol, vol_filtrated),
      c(sizeCols, paste0('lab_chla_tot_vol_rep_', rep), colNames[1])
    ))

    acid_df <- perM2Cols
    acid_df[[colNames[2]]] <- chla_acid_ugL_rep
    chla_acid_ugm2_rep <- as_num(calcChlaPerM2(acid_df))

    noacid_df <- perM2Cols
    noacid_df[[colNames[4]]] <- chla_noacid_ugL_rep
    chla_noacid_ugm2_rep <- as_num(calcChlaPerM2(noacid_df))

    afdm_df <- perM2Cols
    afdm_df[[colNames[6]]] <- afdm_g_filter_rep
    afdm_gm2_rep <- as_num(calcBenthicAFDM(afdm_df))

    put(colNames[3], chla_acid_ugm2_rep)
    put(colNames[5], chla_noacid_ugm2_rep)
    put(colNames[7], afdm_gm2_rep)

    newCols <- setNames(
      data.frame(
        lab_chla_vol_filtrated_rep = vol_filtrated,
        chla_acid_ugL_rep = chla_acid_ugL_rep,
        chla_acid_ugm2_rep = chla_acid_ugm2_rep,
        chla_noacid_ugL_rep = chla_noacid_ugL_rep,
        chla_noacid_ugm2_rep = chla_noacid_ugm2_rep,
        afdm_g_filter_rep = afdm_g_filter_rep,
        afdm_gm2_rep = afdm_gm2_rep
      ),
      colNames
    )

    calculations_chla <- if (is.null(calculations_chla)) {
      newCols
    } else {
      dplyr::bind_cols(calculations_chla, newCols)
    }
  }

  # Portal chla_tool.R:444-492 - avg and sd over the five replicate families.
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

    put(meanCol, as_num(newMean))
    put(sdCol, as_num(newSd))
  }

  out
}
