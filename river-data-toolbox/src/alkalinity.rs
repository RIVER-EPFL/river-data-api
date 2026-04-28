use serde::{Deserialize, Serialize};

/// Result of a Gran titration alkalinity calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlkalinityResult {
    /// Alkalinity in milliequivalents per liter (meq/L).
    pub alkalinity_meq_l: f64,
    /// Alkalinity in mg/L as CaCO₃.
    pub alkalinity_mg_l_caco3: f64,
}

/// Gran titration alkalinity.
///
/// Formula:
///   alkalinity (meq/L) = (acid_normality * titrant_volume_ml) / sample_volume_ml
///   alkalinity (mg/L CaCO₃) = alkalinity_meq_l * 50.04
///
/// `sample_volume_ml` is derived from `sample_weight_g` assuming density ≈ 1 g/mL.
#[must_use]
pub fn gran_titration(
    sample_weight_g: f64,
    acid_normality: f64,
    titrant_volume_ml: f64,
) -> AlkalinityResult {
    let sample_volume_ml = sample_weight_g; // density ≈ 1 g/mL for dilute aqueous
    let alkalinity_meq_l = if sample_volume_ml == 0.0 {
        f64::NAN
    } else {
        (acid_normality * titrant_volume_ml) / sample_volume_ml
    };
    let alkalinity_mg_l_caco3 = alkalinity_meq_l * 50.04;

    AlkalinityResult {
        alkalinity_meq_l,
        alkalinity_mg_l_caco3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-4;

    #[test]
    fn test_gran_titration_basic() {
        // 50mL sample, 0.02N acid, 5mL titrant => 0.02*5/50 = 0.002 meq/L
        let result = gran_titration(50.0, 0.02, 5.0);
        assert!(
            (result.alkalinity_meq_l - 0.002).abs() < TOL,
            "expected 0.002, got {}",
            result.alkalinity_meq_l
        );
        assert!(
            (result.alkalinity_mg_l_caco3 - 0.002 * 50.04).abs() < TOL,
            "expected {}, got {}",
            0.002 * 50.04,
            result.alkalinity_mg_l_caco3
        );
    }

    #[test]
    fn test_gran_titration_zero_sample() {
        let result = gran_titration(0.0, 0.02, 5.0);
        assert!(result.alkalinity_meq_l.is_nan());
    }
}
