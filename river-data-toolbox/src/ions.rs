use serde::{Deserialize, Serialize};

/// Result of an ion charge balance calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChargeBalance {
    /// Sum of cation charges in meq/L.
    pub sum_cations_meq: f64,
    /// Sum of anion charges in meq/L.
    pub sum_anions_meq: f64,
    /// Charge balance percentage: (cations - anions) / (cations + anions) * 100.
    pub balance_percent: f64,
}

/// Molar masses and charges for common ions.
/// Returns (molar_mass, abs_charge) for a given ion name.
fn ion_properties(name: &str) -> Option<(f64, f64)> {
    match name.to_uppercase().as_str() {
        // Cations
        "NA" | "NA+" => Some((22.990, 1.0)),
        "K" | "K+" => Some((39.098, 1.0)),
        "MG" | "MG2+" => Some((24.305, 2.0)),
        "CA" | "CA2+" => Some((40.078, 2.0)),
        "NH4" | "NH4+" => Some((18.039, 1.0)),
        "H" | "H+" => Some((1.008, 1.0)),
        "FE2+" => Some((55.845, 2.0)),
        "MN2+" => Some((54.938, 2.0)),
        // Anions
        "CL" | "CL-" => Some((35.453, 1.0)),
        "SO4" | "SO42-" => Some((96.066, 2.0)),
        "NO3" | "NO3-" => Some((62.004, 1.0)),
        "HCO3" | "HCO3-" => Some((61.017, 1.0)),
        "F" | "F-" => Some((18.998, 1.0)),
        "NO2" | "NO2-" => Some((46.006, 1.0)),
        "PO4" | "PO43-" => Some((94.971, 3.0)),
        _ => None,
    }
}

/// Compute charge balance from measured ion concentrations in mg/L.
///
/// Each entry is (ion_name, concentration_mg_l). The function determines
/// meq/L for each ion using its molar mass and charge, then computes
/// the balance percentage.
///
/// Cations: Na, K, Mg, Ca, NH4, H, Fe2+, Mn2+
/// Anions: Cl, SO4, NO3, HCO3, F, NO2, PO4
///
/// Unknown ion names are skipped.
#[must_use]
pub fn charge_balance(cations: &[(&str, f64)], anions: &[(&str, f64)]) -> ChargeBalance {
    let to_meq = |ions: &[(&str, f64)]| -> f64 {
        ions.iter()
            .filter_map(|(name, conc_mg_l)| {
                ion_properties(name).map(|(molar_mass, charge)| conc_mg_l / molar_mass * charge)
            })
            .sum()
    };

    let sum_cations_meq = to_meq(cations);
    let sum_anions_meq = to_meq(anions);
    let total = sum_cations_meq + sum_anions_meq;

    let balance_percent = if total == 0.0 {
        0.0
    } else {
        (sum_cations_meq - sum_anions_meq) / total * 100.0
    };

    ChargeBalance {
        sum_cations_meq,
        sum_anions_meq,
        balance_percent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_charge_balance_equal() {
        // Na+ at 22.99 mg/L => 1.0 meq/L, Cl- at 35.453 mg/L => 1.0 meq/L
        let result = charge_balance(&[("Na", 22.990)], &[("Cl", 35.453)]);
        assert!(
            result.balance_percent.abs() < 0.1,
            "expected ~0%, got {:.4}%",
            result.balance_percent
        );
    }

    #[test]
    fn test_charge_balance_imbalanced() {
        // High Na with low Cl
        let result = charge_balance(&[("Na", 45.98)], &[("Cl", 35.453)]);
        assert!(
            result.balance_percent > 0.0,
            "expected positive imbalance, got {:.4}%",
            result.balance_percent
        );
    }

    #[test]
    fn test_charge_balance_empty() {
        let result = charge_balance(&[], &[]);
        assert!(
            (result.balance_percent - 0.0).abs() < f64::EPSILON,
            "expected 0%, got {:.4}%",
            result.balance_percent
        );
    }

    #[test]
    fn test_divalent_ions() {
        // Ca2+ at 40.078 mg/L => 2.0 meq/L, SO42- at 96.066 mg/L => 2.0 meq/L
        let result = charge_balance(&[("Ca", 40.078)], &[("SO4", 96.066)]);
        assert!(
            result.balance_percent.abs() < 0.1,
            "expected ~0%, got {:.4}%",
            result.balance_percent
        );
    }
}
