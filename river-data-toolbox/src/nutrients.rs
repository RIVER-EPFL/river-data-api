use std::collections::HashMap;

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

/// Process multiple nutrient species at once.
///
/// Each key in the map is a species name (e.g. "P", "NH4", "NOx", "NO2", "TDP", "TDN"),
/// and the value is a vector of replicates. Returns a map of species name to `NutrientResult`.
///
/// If both "NOx" and "NO2" keys are present (case-insensitive), an additional "NO3" entry
/// is computed element-wise (NOx\[i\] - NO2\[i\]) and then averaged.
#[must_use]
pub fn multi_nutrient_replicates(
    species: &HashMap<String, Vec<f64>>,
) -> HashMap<String, NutrientResult> {
    let mut results = HashMap::new();

    for (name, reps) in species {
        if !reps.is_empty() {
            results.insert(name.clone(), nutrient_from_replicates(reps));
        }
    }

    // Compute NO3 = NOx - NO2 element-wise if both are present
    let nox_key = species
        .keys()
        .find(|k| k.eq_ignore_ascii_case("NOx"));
    let no2_key = species
        .keys()
        .find(|k| k.eq_ignore_ascii_case("NO2"));

    if let (Some(nox_k), Some(no2_k)) = (nox_key, no2_key) {
        let nox_reps = &species[nox_k];
        let no2_reps = &species[no2_k];
        let len = nox_reps.len().min(no2_reps.len());
        if len > 0 {
            let no3_reps: Vec<f64> = (0..len)
                .map(|i| nox_reps[i] - no2_reps[i])
                .collect();
            results.insert("NO3".to_string(), nutrient_from_replicates(&no3_reps));
        }
    }

    results
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

    #[test]
    fn test_multi_nutrient_basic() {
        let mut species = HashMap::new();
        species.insert("P".to_string(), vec![1.0, 2.0, 3.0]);
        species.insert("NH4".to_string(), vec![10.0, 20.0, 30.0]);

        let results = multi_nutrient_replicates(&species);
        assert_eq!(results.len(), 2);
        assert!((results["P"].mean - 2.0).abs() < TOL);
        assert!((results["NH4"].mean - 20.0).abs() < TOL);
    }

    #[test]
    fn test_multi_nutrient_no3_computed() {
        let mut species = HashMap::new();
        species.insert("NOx".to_string(), vec![50.0, 60.0, 70.0]);
        species.insert("NO2".to_string(), vec![3.0, 5.0, 7.0]);

        let results = multi_nutrient_replicates(&species);
        assert!(results.contains_key("NO3"));
        // NO3 = [47, 55, 63], mean = 55.0
        assert!((results["NO3"].mean - 55.0).abs() < TOL);
    }

    #[test]
    fn test_multi_nutrient_no3_not_computed_without_both() {
        let mut species = HashMap::new();
        species.insert("NOx".to_string(), vec![50.0, 60.0]);

        let results = multi_nutrient_replicates(&species);
        assert!(!results.contains_key("NO3"));
    }

    #[test]
    fn test_multi_nutrient_empty_species_skipped() {
        let mut species = HashMap::new();
        species.insert("P".to_string(), vec![1.0, 2.0]);
        species.insert("TDP".to_string(), vec![]);

        let results = multi_nutrient_replicates(&species);
        assert!(results.contains_key("P"));
        assert!(!results.contains_key("TDP"));
    }
}
