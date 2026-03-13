/// Chlorophyll-a with acid correction method.
///
/// From R `calcChlaAcid`:
///   chla = (fluor_before - fluor_after) * slope + intercept
///
/// `fluor_before`: fluorescence reading before acidification
/// `fluor_after`: fluorescence reading after acidification
/// `slope`, `intercept`: from standard curve
#[must_use]
pub fn chla_acid(fluor_before: f64, fluor_after: f64, slope: f64, intercept: f64) -> f64 {
    (fluor_before - fluor_after) * slope + intercept
}

/// Chlorophyll-a without acid (direct fluorescence).
///
/// From R `calcChlaNoAcid`:
///   chla = fluorescence * slope + intercept
#[must_use]
pub fn chla_no_acid(fluorescence: f64, slope: f64, intercept: f64) -> f64 {
    fluorescence * slope + intercept
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-6;

    #[test]
    fn test_chla_acid() {
        // before=100, after=30, slope=0.5, intercept=1.0
        // (100-30)*0.5 + 1.0 = 36.0
        let result = chla_acid(100.0, 30.0, 0.5, 1.0);
        assert!((result - 36.0).abs() < TOL, "expected 36.0, got {result}");
    }

    #[test]
    fn test_chla_no_acid() {
        // fluor=50, slope=0.8, intercept=2.0 => 50*0.8+2.0 = 42.0
        let result = chla_no_acid(50.0, 0.8, 2.0);
        assert!((result - 42.0).abs() < TOL, "expected 42.0, got {result}");
    }

    #[test]
    fn test_chla_acid_zero_diff() {
        // before == after => only intercept
        let result = chla_acid(50.0, 50.0, 0.5, 1.0);
        assert!((result - 1.0).abs() < TOL, "expected 1.0, got {result}");
    }

    #[test]
    fn test_chla_negative_result() {
        // Possible if fluorescence is very low and intercept is negative
        let result = chla_no_acid(1.0, 0.5, -10.0);
        assert!(result < 0.0, "expected negative, got {result}");
    }
}
