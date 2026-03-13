/// Barometric pressure (hPa) estimated from site altitude and air temperature.
///
/// Uses the barometric formula (approximation of `bigleaf::pressure.from.elevation`):
///   P = 1013.25 * (1 - 0.0065 * elevation / (temp_c + 273.15 + 0.0065 * elevation))^5.2561
///
/// Result is in hPa (the R code multiplies by 10 to convert from kPa, but the
/// barometric formula already gives hPa when using the standard constants).
#[must_use]
pub fn barometric_pressure_from_altitude(elevation_m: f64, temp_c: f64) -> f64 {
    let temp_k = temp_c + 273.15;
    let lapse = 0.0065; // K/m
    let base = 1.0 - lapse * elevation_m / (temp_k + lapse * elevation_m);
    let pressure_kpa = 101.325 * base.powf(5.2561);
    // Convert kPa -> hPa (* 10) and round, matching R code: round(bigleaf::pressure.from.elevation(elev, temp) * 10)
    (pressure_kpa * 10.0).round()
}

/// CO2 correction using standard curve + pressure/temperature.
///
/// From R `calcCO2corr`:
///   1. Optionally apply standard curve: raw_co2 = raw_co2 * slope + intercept
///   2. Correct: raw_co2 * pressure_hpa * 298 / (1013 * (273 + temp_c))
///
/// `std_curve`: optional (slope, intercept) pair; pass `None` to skip correction.
#[must_use]
pub fn co2_correction(
    raw_co2: f64,
    pressure_hpa: f64,
    temp_c: f64,
    std_curve: Option<(f64, f64)>,
) -> f64 {
    let corrected = match std_curve {
        Some((slope, intercept)) => raw_co2 * slope + intercept,
        None => raw_co2,
    };
    corrected * pressure_hpa * 298.0 / (1013.0 * (273.0 + temp_c))
}

/// Select the best available pressure value.
///
/// From R pattern: use field_pressure if it's in [700, 1050] hPa, else fall back to altitude_pressure.
#[must_use]
pub fn select_pressure(field_pressure: Option<f64>, altitude_pressure: Option<f64>) -> Option<f64> {
    if let Some(fp) = field_pressure {
        if (700.0..=1050.0).contains(&fp) {
            return Some(fp);
        }
    }
    altitude_pressure
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_barometric_pressure_sea_level() {
        // At 0m, 15°C => ~1013 hPa
        let result = barometric_pressure_from_altitude(0.0, 15.0);
        assert!(
            (result - 1013.0).abs() < 1.0,
            "expected ~1013 at sea level, got {result}"
        );
    }

    #[test]
    fn test_barometric_pressure_high_altitude() {
        // At 2000m, 10°C => roughly 795 hPa
        let result = barometric_pressure_from_altitude(2000.0, 10.0);
        assert!(
            (result - 795.0).abs() < 10.0,
            "expected ~795 at 2000m, got {result}"
        );
    }

    #[test]
    fn test_co2_correction_no_curve() {
        // raw=500, pressure=900, temp=15 => 500 * 900 * 298 / (1013 * 288)
        let result = co2_correction(500.0, 900.0, 15.0, None);
        let expected = 500.0 * 900.0 * 298.0 / (1013.0 * 288.0);
        assert!(
            (result - expected).abs() < 0.001,
            "expected {expected}, got {result}"
        );
    }

    #[test]
    fn test_co2_correction_with_curve() {
        // raw=500, slope=1.1, intercept=-5 => corrected=545
        // 545 * 900 * 298 / (1013 * 288)
        let result = co2_correction(500.0, 900.0, 15.0, Some((1.1, -5.0)));
        let corrected = 500.0 * 1.1 + (-5.0);
        let expected = corrected * 900.0 * 298.0 / (1013.0 * 288.0);
        assert!(
            (result - expected).abs() < 0.001,
            "expected {expected}, got {result}"
        );
    }

    #[test]
    fn test_select_pressure_valid_field() {
        assert_eq!(select_pressure(Some(950.0), Some(800.0)), Some(950.0));
    }

    #[test]
    fn test_select_pressure_out_of_range() {
        assert_eq!(select_pressure(Some(600.0), Some(800.0)), Some(800.0));
    }

    #[test]
    fn test_select_pressure_no_field() {
        assert_eq!(select_pressure(None, Some(800.0)), Some(800.0));
    }
}
