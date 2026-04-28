/// SUVA (Specific UV Absorbance) at 254nm.
///
/// From R `calcSUVA`:
///   SUVA = a254 * 1000 / DOC_avg_ppb
///
/// Result is in L/(mg·m). DOC must be in ppb (µg/L).
/// Returns NaN if DOC is zero.
#[must_use]
pub fn suva(a254: f64, doc_avg_ppb: f64) -> f64 {
    if doc_avg_ppb == 0.0 {
        return f64::NAN;
    }
    a254 * 1000.0 / doc_avg_ppb
}

/// Generic absorbance ratio (E2:E3, E4:E6, spectral slope ratio).
///
/// From R `calcRatio`. Returns NaN if denominator is zero.
#[must_use]
pub fn absorbance_ratio(numerator: f64, denominator: f64) -> f64 {
    crate::common::ratio(numerator, denominator)
}

/// Spectral slope between two wavelengths.
///
/// S = -ln(a_λ1 / a_λ2) / (λ2 - λ1)
///
/// Both absorbances must be positive. Returns NaN otherwise.
#[must_use]
pub fn spectral_slope(abs_short: f64, abs_long: f64, wl_short_nm: f64, wl_long_nm: f64) -> f64 {
    if abs_short <= 0.0 || abs_long <= 0.0 || (wl_long_nm - wl_short_nm) == 0.0 {
        return f64::NAN;
    }
    -(abs_short / abs_long).ln() / (wl_long_nm - wl_short_nm)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-6;

    #[test]
    fn test_suva_basic() {
        // a254=0.1, DOC=2000 ppb => SUVA = 0.1 * 1000 / 2000 = 0.05
        let result = suva(0.1, 2000.0);
        assert!((result - 0.05).abs() < TOL, "expected 0.05, got {result}");
    }

    #[test]
    fn test_suva_zero_doc() {
        assert!(suva(0.1, 0.0).is_nan());
    }

    #[test]
    fn test_absorbance_ratio() {
        // E2:E3 = a250/a365
        let result = absorbance_ratio(0.2, 0.05);
        assert!((result - 4.0).abs() < TOL);
    }

    #[test]
    fn test_spectral_slope() {
        // S275-295: abs at 275nm and 295nm
        // If abs_275 = 0.2, abs_295 = 0.1 => S = -ln(0.2/0.1)/(295-275) = -ln(2)/20 ≈ -0.03466
        // But S is defined as negative log ratio / positive Δλ, giving negative when abs decreases
        let result = spectral_slope(0.2, 0.1, 275.0, 295.0);
        let expected = -(0.2_f64 / 0.1).ln() / 20.0;
        assert!(
            (result - expected).abs() < TOL,
            "expected {expected}, got {result}"
        );
    }

    #[test]
    fn test_spectral_slope_equal_wavelengths() {
        assert!(spectral_slope(0.2, 0.1, 275.0, 275.0).is_nan());
    }
}
