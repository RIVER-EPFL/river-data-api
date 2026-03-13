/// Mean of a slice, ignoring NaN values.
/// Returns NaN if the slice is empty or all NaN.
#[must_use]
pub fn mean(values: &[f64]) -> f64 {
    let valid: Vec<f64> = values.iter().copied().filter(|v| !v.is_nan()).collect();
    if valid.is_empty() {
        return f64::NAN;
    }
    valid.iter().sum::<f64>() / valid.len() as f64
}

/// Sample standard deviation (Bessel-corrected, N-1 denominator).
/// Returns NaN if fewer than 2 valid values.
#[must_use]
pub fn std_dev(values: &[f64]) -> f64 {
    let valid: Vec<f64> = values.iter().copied().filter(|v| !v.is_nan()).collect();
    if valid.len() < 2 {
        return f64::NAN;
    }
    let m = valid.iter().sum::<f64>() / valid.len() as f64;
    let variance = valid.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (valid.len() - 1) as f64;
    variance.sqrt()
}

/// Apply a standard curve correction: corrected = raw * slope + intercept.
/// This matches the CNET R pattern: `raw * stdCurve$a + stdCurve$b`.
#[must_use]
pub fn apply_standard_curve(raw: f64, slope: f64, intercept: f64) -> f64 {
    raw * slope + intercept
}

/// Difference of two values (R `calcMinus`).
#[must_use]
pub fn minus(a: f64, b: f64) -> f64 {
    a - b
}

/// Ratio of two values with zero-denominator guard (R `calcRatio`).
/// Returns NaN if denominator is zero.
#[must_use]
pub fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator == 0.0 {
        return f64::NAN;
    }
    numerator / denominator
}

/// Return first value if not NaN, else fallback (R `calcEquals`).
#[must_use]
pub fn equals(primary: f64, fallback: f64) -> f64 {
    if primary.is_nan() { fallback } else { primary }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-10;

    #[test]
    fn test_mean_basic() {
        let result = mean(&[1.0, 2.0, 3.0]);
        assert!((result - 2.0).abs() < TOL, "expected 2.0, got {result}");
    }

    #[test]
    fn test_mean_with_nan() {
        let result = mean(&[1.0, f64::NAN, 3.0]);
        assert!((result - 2.0).abs() < TOL, "expected 2.0, got {result}");
    }

    #[test]
    fn test_mean_empty() {
        assert!(mean(&[]).is_nan());
    }

    #[test]
    fn test_std_dev_basic() {
        // sd(c(2, 4, 4, 4, 5, 5, 7, 9)) = 2.138090
        let result = std_dev(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        assert!(
            (result - 2.138_089_935_299_395).abs() < 1e-6,
            "expected ~2.138, got {result}"
        );
    }

    #[test]
    fn test_std_dev_single_value() {
        assert!(std_dev(&[5.0]).is_nan());
    }

    #[test]
    fn test_apply_standard_curve() {
        // raw=100, slope=1.05, intercept=-2.3 => 100*1.05 + (-2.3) = 102.7
        let result = apply_standard_curve(100.0, 1.05, -2.3);
        assert!((result - 102.7).abs() < TOL);
    }

    #[test]
    fn test_minus() {
        assert!((minus(10.0, 3.0) - 7.0).abs() < TOL);
    }

    #[test]
    fn test_ratio_normal() {
        assert!((ratio(10.0, 4.0) - 2.5).abs() < TOL);
    }

    #[test]
    fn test_ratio_zero_denominator() {
        assert!(ratio(10.0, 0.0).is_nan());
    }

    #[test]
    fn test_equals_primary_valid() {
        assert!((equals(5.0, 10.0) - 5.0).abs() < TOL);
    }

    #[test]
    fn test_equals_primary_nan() {
        assert!((equals(f64::NAN, 10.0) - 10.0).abs() < TOL);
    }
}
