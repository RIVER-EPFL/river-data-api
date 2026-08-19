# chla_benthic: the portal Chl a tool chain (modules/tools_tab/tools/chla_tool.R:274-496) driven
# by a replicate grid instead of the portal's fixed A..E bench columns.
#
# Per replicate the portal computes calcMinus (volume filtrated), calcChlaAcid, calcChlaNoAcid,
# calcChlaPerM2 for each chla variant and calcBenthicAFDM, then loops five parameter families
# writing a calcMean and a calcSd column for each (chla_tool.R:440-492). The grid row carries the
# AFDM already differenced, where the portal derives it from lab_chla_wgt_1_rep_ minus
# lab_chla_wgt_2_rep_ (chla_tool.R:353-363); everything downstream of that is identical.
#
# Output keys are the portal's columns, with the replicate letter taken from the row's position
# in the grid.
#
# 'KEEP OLD' is the portal's "leave the stored cell as it was" sentinel, which it resolves by
# reading the value back out of row(). This tool is stateless, so there is no old value and the
# key is omitted. The sentinel is tested before coercion: as.numeric('KEEP OLD') is NA and could
# not then be told apart from a computed NaN.

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
  # The portal's ifelse(x != 'KEEP OLD', x, pull(row(), col)) with no row() to fall back on. Only
  # the sentinel becomes NA: the comparison is against a string, so NaN and Inf test TRUE and are
  # stored and propagated into the downstream conversions and the avg/sd.
  keep_old <- function(x) {
    if (is.character(x) || length(x) != 1L) return(NA_real_)
    as.numeric(x)
  }
  # A cell the portal leaves unchanged or writes NA into has no stateless equivalent, so its key
  # is omitted. NaN and Inf are values the portal displays, so they are emitted and reach the
  # caller as JSON null.
  emit <- function(x) is.numeric(x) && length(x) == 1L && (!is.na(x) || is.nan(x))

  # The portal's two selectInputs (chla_tool.R:40-61) resolve to chla_acid_std_curve_id /
  # chla_noacid_std_curve_id, or NA when left on 'Select a date...'. Coefficients typed into the
  # request stand in for a curve the curves table does not hold.
  curve_coefs <- function(curve, slope_in, intercept_in) {
    df <- riverdata.tools::curve_df(curve)
    if (!is.null(df)) c(num(df$a), num(df$b)) else c(num(slope_in), num(intercept_in))
  }
  acid_ab <- curve_coefs(curves$chla_acid, inputs$acid_slope, inputs$acid_intercept)
  noacid_ab <- curve_coefs(curves$chla_noacid, inputs$noacid_slope, inputs$noacid_intercept)

  curve_rows <- data.frame(id = numeric(0), a = numeric(0), b = numeric(0))
  acid_id <- NA_real_
  noacid_id <- NA_real_
  if (!any(is.na(acid_ab))) {
    acid_id <- 1
    curve_rows <- rbind(curve_rows, data.frame(id = 1, a = acid_ab[1], b = acid_ab[2]))
  }
  if (!any(is.na(noacid_ab))) {
    noacid_id <- 2
    curve_rows <- rbind(curve_rows, data.frame(id = 2, a = noacid_ab[1], b = noacid_ab[2]))
  }
  pool <- list(standard_curves = curve_rows)

  reps <- inputs$replicates
  if (is.data.frame(reps)) {
    reps <- lapply(seq_len(nrow(reps)), function(i) {
      lapply(reps, function(col) {
        if (is.matrix(col)) col[i, ] else if (is.list(col)) col[[i]] else col[i]
      })
    })
  }
  if (is.null(reps) || length(reps) == 0L) return(list())

  ## Per-replicate calculations (chla_tool.R:276-438) ############################
  calculations_chla <- NULL

  for (i in seq_along(reps)) {
    r <- reps[[i]]
    rep <- LETTERS[i]

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

    fluor_1 <- num(r$fluor_before)
    fluor_2 <- num(r$fluor_after)
    tot_vol <- num(r$vol_total_ml)
    vol_after <- num(r$vol_after_ml)
    afdm_g_filter_rep <- num(r$afdm_g_filter)
    # The portal reads exactly sizeA/sizeB/sizeC: a grid row carrying fewer than three diameters
    # leaves the per-m2 chain at 'KEEP OLD', and a fourth is a column the portal does not have.
    d <- suppressWarnings(as.numeric(unlist(r$diameters_cm)))
    sizes <- c(d, rep(NA_real_, 3L))[1:3]

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

    # chla_tool.R:365-378
    perM2Cols <- riverdata.tools::row_df(setNames(
      list(sizes[1], sizes[2], sizes[3], tot_vol, lab_chla_vol_filtrated_rep),
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
    chla_acid_ugm2_rep <- keep_old(calcChlaPerM2(with_col(colNames[2], chla_acid_ugL_rep)))
    chla_noacid_ugm2_rep <- keep_old(calcChlaPerM2(with_col(colNames[4], chla_noacid_ugL_rep)))

    # chla_tool.R:403-413
    afdm_gm2_rep <- keep_old(calcBenthicAFDM(with_col(colNames[6], afdm_g_filter_rep)))

    # chla_tool.R:415-437, without the afdm_g_filter_rep column: the grid sends that value in
    # rather than deriving it, so restating it as a result would echo the request.
    newCols <- riverdata.tools::row_df(setNames(
      list(lab_chla_vol_filtrated_rep,
           chla_acid_ugL_rep,
           chla_acid_ugm2_rep,
           chla_noacid_ugL_rep,
           chla_noacid_ugm2_rep,
           afdm_gm2_rep),
      colNames[c(1, 2, 3, 4, 5, 7)]
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
