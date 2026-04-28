/// Deuterium excess: d-excess = δD - 8 × δ¹⁸O.
///
/// Standard definition from Dansgaard (1964).
#[must_use]
pub fn deuterium_excess(d_d: f64, d18o: f64) -> f64 {
    d_d - 8.0 * d18o
}

/// ¹⁷O excess (Δ'¹⁷O) in per meg.
///
/// Δ'¹⁷O = δ'¹⁷O - 0.528 × δ'¹⁸O
/// where δ' = ln(1 + δ/1000) × 1000
///
/// Convention: result in per meg (multiply by 1000).
#[must_use]
pub fn o17_excess(d17o_permil: f64, d18o_permil: f64) -> f64 {
    let d17_prime = (1.0 + d17o_permil / 1000.0).ln() * 1000.0;
    let d18_prime = (1.0 + d18o_permil / 1000.0).ln() * 1000.0;
    (d17_prime - 0.528 * d18_prime) * 1000.0 // per meg
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-4;

    #[test]
    fn test_deuterium_excess_gmwl() {
        // On GMWL: δD = 8*δ18O + 10, so d-excess = 10
        // δ18O = -10, δD = 8*(-10) + 10 = -70
        let result = deuterium_excess(-70.0, -10.0);
        assert!((result - 10.0).abs() < TOL, "expected 10.0, got {result}");
    }

    #[test]
    fn test_deuterium_excess_evaporated() {
        // Evaporated water: δ18O = -5, δD = -30 => d-excess = -30 - 8*(-5) = 10
        let result = deuterium_excess(-30.0, -5.0);
        assert!((result - 10.0).abs() < TOL, "expected 10.0, got {result}");
    }

    #[test]
    fn test_o17_excess() {
        // Typical meteoric water: d17O ≈ -5.3, d18O ≈ -10
        let result = o17_excess(-5.3, -10.0);
        // Should be a small value in per meg
        assert!(result.is_finite(), "expected finite value, got {result}");
    }

    #[test]
    fn test_o17_excess_zero() {
        // At zero isotope ratios, excess should be zero
        let result = o17_excess(0.0, 0.0);
        assert!(result.abs() < 0.1, "expected ~0, got {result}");
    }
}
