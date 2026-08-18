tool <- function(inputs, constants, curves) {
  results <- list()

  one_row_df <- function(vals) {
    as.data.frame(as.list(stats::setNames(vals, paste0("r", seq_along(vals)))))
  }

  add_avg_sd <- function(vals, avg_key, sd_key) {
    df <- one_row_df(vals)
    avg <- calcMean(df)
    if (is.numeric(avg) && !is.na(avg)) results[[avg_key]] <<- avg
    stdev <- calcSd(df)
    if (is.numeric(stdev) && !is.na(stdev)) results[[sd_key]] <<- stdev
  }

  # Pad or truncate to the portal's replicate letters A, B, C
  reps_abc <- function(v) {
    x <- rep(NA_real_, 3)
    v <- suppressWarnings(as.numeric(unlist(v)))
    n <- min(length(v), 3)
    if (n > 0) x[seq_len(n)] <- v[seq_len(n)]
    x
  }

  species <- inputs$species
  if (!is.null(species)) {
    for (name in names(species)) {
      vals <- suppressWarnings(as.numeric(unlist(species[[name]])))
      if (length(vals) == 0) next
      if (name == "NH4") {
        avg_key <- "NH4_avg_ugL"
        sd_key <- "NH4_sd_ugL"
      } else if (name == "SRP") {
        avg_key <- "SRP_avg_ugL"
        sd_key <- "SRP_sd_ugL"
      } else {
        base <- sub("^NUT_", "", name)
        avg_key <- paste0("NUT_", base, "_avg")
        sd_key <- paste0("NUT_", base, "_sd")
      }
      add_avg_sd(vals, avg_key, sd_key)
    }

    # NO3 per replicate letter: calcMinus(NOx_rep, NO2_rep), NA propagates
    keys <- names(species)
    base_lower <- tolower(sub("^NUT_", "", keys))
    nox_i <- match("nox", base_lower)
    no2_i <- match("no2", base_lower)
    if (!is.na(nox_i) && !is.na(no2_i)) {
      nox <- reps_abc(species[[keys[nox_i]]])
      no2 <- reps_abc(species[[keys[no2_i]]])
      no3 <- vapply(
        1:3,
        function(i) calcMinus(data.frame(a = nox[i], b = no2[i])),
        numeric(1)
      )
      add_avg_sd(no3, "NUT_NO3_avg", "NUT_NO3_sd")
    }
  } else if (!is.null(inputs$replicates)) {
    vals <- suppressWarnings(as.numeric(unlist(inputs$replicates)))
    if (length(vals) > 0) {
      add_avg_sd(vals, "NUT_avg", "NUT_sd")
    }
    if (!is.null(inputs$nox) && !is.null(inputs$no2)) {
      no3 <- calcMinus(data.frame(
        a = suppressWarnings(as.numeric(inputs$nox)),
        b = suppressWarnings(as.numeric(inputs$no2))
      ))
      if (!is.na(no3)) results[["NUT_NO3_avg"]] <- no3
    }
  }

  results
}
