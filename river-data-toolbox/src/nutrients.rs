use crate::common;
use serde::{Deserialize, Serialize};

/// Result from replicate nutrient measurements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NutrientResult {
    pub mean: f64,
    pub std_dev: f64,
}

/// Compute mean and standard deviation from nutrient replicates.
///
/// From R `calcMean`/`calcSd` applied to nutrient measurement replicates.
#[must_use]
pub fn nutrient_from_replicates(replicates: &[f64]) -> NutrientResult {
    NutrientResult {
        mean: common::mean(replicates),
        std_dev: common::std_dev(replicates),
    }
}

/// Nitrate from NOx and NO2: NO3 = NOx - NO2.
///
/// From R `calcMinus` pattern applied to nitrogen species.
#[must_use]
pub fn nitrate_from_nox_no2(nox: f64, no2: f64) -> f64 {
    nox - no2
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-6;

    #[test]
    fn test_nutrient_replicates() {
        let result = nutrient_from_replicates(&[10.0, 12.0, 11.0]);
        assert!((result.mean - 11.0).abs() < TOL);
        assert!(result.std_dev > 0.0);
        // sd(c(10,12,11)) = 1.0
        assert!((result.std_dev - 1.0).abs() < TOL);
    }

    #[test]
    fn test_nitrate() {
        assert!((nitrate_from_nox_no2(50.0, 3.0) - 47.0).abs() < TOL);
    }

    #[test]
    fn test_nutrient_single_replicate() {
        let result = nutrient_from_replicates(&[5.0]);
        assert!((result.mean - 5.0).abs() < TOL);
        assert!(result.std_dev.is_nan());
    }
}
