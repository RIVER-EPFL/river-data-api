tool <- function(inputs, constants, curves) {
  results <- list()

  # The portal's replicate letters (nutrients_tool.R:202)
  reps <- c('A', 'B', 'C')

  # One cell. The input name IS the portal column, so a replicate a caller did
  # not supply stays NA at its own letter and cannot be shifted onto another.
  cell <- function(name) {
    v <- suppressWarnings(as.numeric(unlist(inputs[[name]])))
    if (length(v) == 0) NA_real_ else v[1]
  }

  rep_df <- function(param) {
    cols <- paste0(param, '_rep_', reps)
    stats::setNames(as.data.frame(lapply(cols, cell)), cols)
  }

  # nutrients_tool.R:100-101 -- the editable raw table is the NUT_* replicates
  # WITHOUT the NUT_NO3 replicates, so a supplied NO3 replicate can never
  # reach the calculation. The portal's column set is whatever the
  # grab_param_categories rows for Nutrients / Old Nutrients hold (:69-79); the
  # six below are the NUT_ bases the calculation loop at :237 names.
  rawData <- dplyr::bind_cols(lapply(
    c('NUT_P', 'NUT_NH4', 'NUT_NOx', 'NUT_NO2', 'NUT_TDP', 'NUT_TDN'),
    rep_df
  ))

  # nutrients_tool.R:115 -- the Old Nutrients table
  oldNut <- dplyr::bind_cols(lapply(c('NH4', 'SRP'), rep_df))

  # Calculate NO3 (nutrients_tool.R:198-228)
  no3 <- NULL
  for (rep in reps) {
    repName <- paste0('NUT_NO3_rep_', rep)
    newCols <- stats::setNames(
      data.frame(
        repName = calcMinus(
          select(
            rawData,
            (starts_with('NUT_NOx_rep_') | starts_with('NUT_NO2_rep_')) & ends_with(rep)
          )
        )
      ),
      repName
    )
    if (is.null(no3)) {
      no3 <- newCols
    } else {
      no3 <- bind_cols(no3, newCols)
    }
  }

  # The derived NO3 replicates are portal columns in their own right: the tool's
  # returned df carries no3Updated() (nutrients_tool.R:328) and entry_layout.R:401-407
  # writes every non-NA column of it back to the data table. The portal's own drop
  # is is.na() (entry_layout.R:407), which also removes NaN, so the test here
  # matches it rather than testing a sentinel: calcMinus returns numeric NA, never
  # 'KEEP OLD' (calculation_functions.R:14-32).
  for (rep in reps) {
    repName <- paste0('NUT_NO3_rep_', rep)
    v <- no3[[repName]]
    if (!is.na(v)) results[[repName]] <- as.numeric(v)
  }

  # Calculate nutrients avg and sd (nutrients_tool.R:230-299)
  oldNutrients <- c('NH4', 'SRP')

  for (param in c('NUT_P', 'NUT_NH4', 'NUT_NOx', 'NUT_NO2', 'NUT_NO3', 'NUT_TDP', 'NUT_TDN', 'NH4', 'SRP')) {
    if (param == 'NUT_NO3') {
      df <- no3
    } else {
      if (param %in% oldNutrients) {
        df <- oldNut %>% select(starts_with(param))
      } else {
        df <- rawData %>% select(starts_with(param))
      }
    }

    newMean <- calcMean(df)
    newSd <- calcSd(df)
    if (param %in% oldNutrients) {
      meanCol <- paste0(param, '_avg_ugL')
      sdCol <- paste0(param, '_sd_ugL')
    } else {
      meanCol <- paste0(param, '_avg')
      sdCol <- paste0(param, '_sd')
    }

    # 'KEEP OLD' carries the stored value forward in the portal; this tool is
    # stateless, so the key is omitted instead.
    if (!identical(newMean, 'KEEP OLD')) results[[meanCol]] <- as.numeric(newMean)
    if (!identical(newSd, 'KEEP OLD')) results[[sdCol]] <- as.numeric(newSd)
  }

  results
}
