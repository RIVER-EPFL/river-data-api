use crate::common;

/// DOC replicate corrected with optional standard curve.
///
/// From R `calcDOC`: if std_curve is present, apply `raw * slope + intercept`.
#[must_use]
pub fn doc_corrected(raw: f64, std_curve: Option<(f64, f64)>) -> f64 {
    match std_curve {
        Some((slope, intercept)) => common::apply_standard_curve(raw, slope, intercept),
        None => raw,
    }
}

/// DOC average from replicates (typically 3), with optional standard curve correction.
///
/// From R `calcDOCavg`: correct each replicate, then take mean.
#[must_use]
pub fn doc_average(replicates: &[f64], std_curve: Option<(f64, f64)>) -> f64 {
    let corrected: Vec<f64> = replicates
        .iter()
        .map(|&r| doc_corrected(r, std_curve))
        .collect();
    common::mean(&corrected)
}

/// DOC standard deviation from replicates, with optional standard curve correction.
///
/// From R `calcDOCsd`.
#[must_use]
pub fn doc_std_dev(replicates: &[f64], std_curve: Option<(f64, f64)>) -> f64 {
    let corrected: Vec<f64> = replicates
        .iter()
        .map(|&r| doc_corrected(r, std_curve))
        .collect();
    common::std_dev(&corrected)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-6;

    #[test]
    fn test_doc_corrected_no_curve() {
        assert!((doc_corrected(5.5, None) - 5.5).abs() < TOL);
    }

    #[test]
    fn test_doc_corrected_with_curve() {
        // raw=5.5, slope=1.02, intercept=-0.1 => 5.5*1.02 - 0.1 = 5.51
        let result = doc_corrected(5.5, Some((1.02, -0.1)));
        assert!((result - 5.51).abs() < TOL, "expected 5.51, got {result}");
    }

    #[test]
    fn test_doc_average_no_curve() {
        let reps = [3.0, 4.0, 5.0];
        let result = doc_average(&reps, None);
        assert!((result - 4.0).abs() < TOL);
    }

    #[test]
    fn test_doc_average_with_curve() {
        // slope=2.0, intercept=0.0 => each replicate doubled
        let reps = [1.0, 2.0, 3.0];
        let result = doc_average(&reps, Some((2.0, 0.0)));
        assert!((result - 4.0).abs() < TOL);
    }

    #[test]
    fn test_doc_std_dev() {
        let reps = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let result = doc_std_dev(&reps, None);
        assert!(
            (result - 2.138_089_935_299_395).abs() < 1e-4,
            "expected ~2.138, got {result}"
        );
    }
}
