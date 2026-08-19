# Slug-injection discharge (CNET portal discharge_tool.R calculation section).
# The prelude has no discharge function, so the portal calculation lives here.
tool <- function(inputs, constants, curves) {
  library(pracma)
  library(signal)

  num <- function(x, default = NA_real_) {
    if (is.null(x) || length(x) == 0 || all(is.na(x))) default else as.numeric(x)[1]
  }
  vec <- function(x) if (is.null(x)) numeric(0) else as.numeric(unlist(x))

  tracer <- if (is.null(inputs$tracer)) 'rhodamine' else as.character(inputs$tracer)[1]
  times <- vec(inputs$times_s)
  values <- vec(inputs$values)

  initial_mass_rhodamine_wt <- num(inputs$initial_mass_rhodamine_wt_g, 3.38019)
  concentration_rhodamine_wt <- num(inputs$concentration_rhodamine_pct, 23.83) / 100
  initial_water_temp_degC <- num(inputs$initial_water_temp_c, 3.3)
  n_rhodamine <- num(inputs$n_rhodamine, 0.026)
  initial_mass_salt <- num(inputs$initial_mass_salt_g, 2000)
  slope_conductivity <- num(inputs$slope_conductivity, 1951.1)
  distance <- num(inputs$distance_m, 79)
  T_ref <- num(inputs$t_ref_c, 25)

  df <- data.frame(time = times, y = values)
  n_rows <- nrow(df)

  # Portal background window: first 15 (rhodamine) / 10 (salt) plus last 10 rows,
  # duplicates kept. On a constant time regressor lm drops the slope and the
  # background is the window mean, as in the portal.
  head_n <- if (identical(tracer, 'salt')) 10 else 15
  background_indices <- c(1:min(head_n, n_rows), max(1, n_rows - 9):n_rows)
  background_model <- lm(y ~ time, data = df[background_indices, ])
  background_predicted <- suppressWarnings(predict(background_model, newdata = df))
  corrected <- df$y - background_predicted

  if (identical(tracer, 'salt')) {
    concentration <- corrected / slope_conductivity
    mass <- initial_mass_salt
    auc_divisor <- 1
  } else {
    concentration <- corrected * exp(n_rhodamine * (initial_water_temp_degC - T_ref))
    mass <- initial_mass_rhodamine_wt * concentration_rhodamine_wt * 1000
    auc_divisor <- 1000
  }

  smoothed <- sgolayfilt(concentration, p = 3, n = min(11, n_rows))
  smoothed[smoothed < 0] <- 0

  time_seconds <- times - times[1]
  auc <- trapz(time_seconds, smoothed) / auc_divisor
  discharge <- mass / auc

  peak_concentration <- max(smoothed)
  travel_time <- time_seconds[which.max(smoothed)]
  velocity <- distance / travel_time

  out <- list()
  # The portal prints all four metrics whatever they come out as, so only a genuine NA is
  # dropped here: NaN and Inf are values it displays and are emitted alike.
  keep <- function(v) {
    is.numeric(v) && length(v) == 1 && (!is.na(v) || is.nan(v))
  }
  if (keep(discharge)) out$Q_Ls <- discharge
  if (keep(velocity)) out$velocity_ms <- velocity
  if (keep(travel_time)) out$travel_time_s <- travel_time
  if (keep(peak_concentration)) out$peak_concentration <- peak_concentration
  out
}
