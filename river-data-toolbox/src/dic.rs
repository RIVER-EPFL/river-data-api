use serde::{Deserialize, Serialize};

/// Result of a DIC calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DICResult {
    /// DIC concentration in µmol/L.
    pub dic_umol_l: f64,
    /// δ13C-DIC in ‰ (permil).
    pub d13c_dic_permil: f64,
}

/// DIC-specific constants, typically fetched from the `constants` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DICConstants {
    /// Henry's law constant for CO2 at 298.15K (mol/(L·atm)), e.g. 0.034.
    pub h_co2_29815k: f64,
    /// Universal gas constant R (J/(mol·K)), 8.314.
    pub gas_const_r_mol: f64,
    /// Vial volume in mL.
    pub vial_volume: f64,
    /// Volume of H3PO4 acid added in mL.
    pub h3po4_added: f64,
}

/// DIC concentration from acid digestion + Picarro CO2 analysis.
///
/// From R `calcDIC`. Intermediate variables:
///   sampleV = acid_sample_weight - acid_weight
///   hsV = vial_volume + overpressure - (sampleV + h3po4_added)
///   co2_acid = co2_dry * (SA_added + hsV)
///   gas_temp = R * T_lab_K
///   exponent = exp(2392.86 * (1/T_lab_K - 1/298.15))
///   DIC = co2_acid * (H * exponent * sampleV * gas_temp + 101.325 * hsV) / (10^3 * gas_temp * hsV * sampleV)
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn dic_concentration(
    acid_sample_weight_g: f64,
    acid_weight_g: f64,
    vol_overpressure_ml: f64,
    sa_added_ml: f64,
    co2_dry_ppm: f64,
    lab_temp_c: f64,
    constants: &DICConstants,
) -> f64 {
    let t_lab_k = lab_temp_c + 273.15;
    let sample_v = acid_sample_weight_g - acid_weight_g;
    let hs_v = constants.vial_volume + vol_overpressure_ml - (sample_v + constants.h3po4_added);
    let co2_acid = co2_dry_ppm * (sa_added_ml + hs_v);
    let gas_temp = constants.gas_const_r_mol * t_lab_k;
    let exponent = (2392.86 * (1.0 / t_lab_k - 1.0 / 298.15)).exp();

    let dividend =
        co2_acid * (constants.h_co2_29815k * exponent * sample_v * gas_temp + 101.325 * hs_v);
    let divisor = 1e3 * gas_temp * hs_v * sample_v;

    if divisor == 0.0 {
        return f64::NAN;
    }
    dividend / divisor
}

/// δ13C-DIC from acid digestion + Picarro isotope analysis.
///
/// From R `calcd13DIC`. The isotope fractionation between dissolved and gaseous CO2
/// is corrected using Henry's law temperature dependence.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn d13c_dic(
    acid_sample_weight_g: f64,
    acid_weight_g: f64,
    vol_overpressure_ml: f64,
    d13co2_permil: f64,
    lab_temp_c: f64,
    constants: &DICConstants,
) -> f64 {
    let t_lab_k = lab_temp_c + 273.15;
    let sample_v = acid_sample_weight_g - acid_weight_g;
    let hs_v = constants.vial_volume + vol_overpressure_ml - (sample_v + constants.h3po4_added);
    let exponent = (2392.86 * (1.0 / t_lab_k - 1.0 / 298.15)).exp();

    let h_cst_expo_sampl_gas =
        constants.h_co2_29815k * exponent * sample_v * constants.gas_const_r_mol;

    let dividend = d13co2_permil * 101.325 * hs_v
        + (t_lab_k * (d13co2_permil + 0.19) - 373.0) * h_cst_expo_sampl_gas;
    let divisor = 101.325 * hs_v + h_cst_expo_sampl_gas * t_lab_k;

    if divisor == 0.0 {
        return f64::NAN;
    }
    dividend / divisor
}

/// Result of replicate DIC calculations (A + B).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DicReplicateResult {
    pub dic_a: f64,
    pub dic_b: f64,
    pub dic_avg: f64,
    pub dic_std: f64,
    pub d13c_a: Option<f64>,
    pub d13c_b: Option<f64>,
    pub d13c_avg: Option<f64>,
    pub d13c_std: Option<f64>,
}

/// Compute DIC and optional δ13C-DIC for two replicates, returning individual values plus avg/SD.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn dic_replicates(
    // Replicate A inputs
    a_acid_sample_wt: f64,
    a_acid_wt: f64,
    a_overpressure: f64,
    a_sa_added: f64,
    a_co2_dry: f64,
    a_d13co2: Option<f64>,
    // Replicate B inputs
    b_acid_sample_wt: f64,
    b_acid_wt: f64,
    b_overpressure: f64,
    b_sa_added: f64,
    b_co2_dry: f64,
    b_d13co2: Option<f64>,
    // Shared
    lab_temp_c: f64,
    constants: &DICConstants,
) -> DicReplicateResult {
    let dic_a = dic_concentration(a_acid_sample_wt, a_acid_wt, a_overpressure, a_sa_added, a_co2_dry, lab_temp_c, constants);
    let dic_b = dic_concentration(b_acid_sample_wt, b_acid_wt, b_overpressure, b_sa_added, b_co2_dry, lab_temp_c, constants);

    let dic_avg = crate::common::mean(&[dic_a, dic_b]);
    let dic_std = crate::common::std_dev(&[dic_a, dic_b]);

    let d13c_a = a_d13co2.map(|d| d13c_dic(a_acid_sample_wt, a_acid_wt, a_overpressure, d, lab_temp_c, constants));
    let d13c_b = b_d13co2.map(|d| d13c_dic(b_acid_sample_wt, b_acid_wt, b_overpressure, d, lab_temp_c, constants));

    let (d13c_avg, d13c_std) = match (d13c_a, d13c_b) {
        (Some(a), Some(b)) => (
            Some(crate::common::mean(&[a, b])),
            Some(crate::common::std_dev(&[a, b])),
        ),
        _ => (None, None),
    };

    DicReplicateResult {
        dic_a,
        dic_b,
        dic_avg,
        dic_std,
        d13c_a,
        d13c_b,
        d13c_avg,
        d13c_std,
    }
}

/// Convenience: compute both DIC and δ13C-DIC together.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn dic(
    acid_sample_weight_g: f64,
    acid_weight_g: f64,
    vol_overpressure_ml: f64,
    sa_added_ml: f64,
    co2_dry_ppm: f64,
    d13co2_permil: f64,
    lab_temp_c: f64,
    constants: &DICConstants,
) -> DICResult {
    DICResult {
        dic_umol_l: dic_concentration(
            acid_sample_weight_g,
            acid_weight_g,
            vol_overpressure_ml,
            sa_added_ml,
            co2_dry_ppm,
            lab_temp_c,
            constants,
        ),
        d13c_dic_permil: d13c_dic(
            acid_sample_weight_g,
            acid_weight_g,
            vol_overpressure_ml,
            d13co2_permil,
            lab_temp_c,
            constants,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_constants() -> DICConstants {
        DICConstants {
            h_co2_29815k: 0.034,
            gas_const_r_mol: 8.314,
            vial_volume: 12.0,
            h3po4_added: 0.1,
        }
    }

    #[test]
    fn test_dic_concentration_positive() {
        let c = test_constants();
        // Typical values: sample_wt=15g, acid_wt=5g (sampleV=10), overpressure=0.5, SA=5, CO2=500ppm, T=22°C
        let result = dic_concentration(15.0, 5.0, 0.5, 5.0, 500.0, 22.0, &c);
        assert!(
            result > 0.0 && result.is_finite(),
            "DIC should be positive, got {result}"
        );
    }

    #[test]
    fn test_d13c_dic_finite() {
        let c = test_constants();
        let result = d13c_dic(15.0, 5.0, 0.5, -15.0, 22.0, &c);
        assert!(result.is_finite(), "d13C-DIC should be finite, got {result}");
    }

    #[test]
    fn test_dic_zero_sample_volume() {
        let c = test_constants();
        // acid_sample_weight == acid_weight => sampleV = 0 => division by zero
        let result = dic_concentration(5.0, 5.0, 0.5, 5.0, 500.0, 22.0, &c);
        assert!(result.is_nan(), "expected NaN for zero sample volume");
    }

    #[test]
    fn test_dic_replicates_avg_between_a_and_b() {
        let c = test_constants();
        // Use slightly different inputs for A and B to get different DIC values
        let result = dic_replicates(
            15.0, 5.0, 0.5, 5.0, 500.0, Some(-15.0), // A
            15.5, 5.0, 0.6, 5.0, 520.0, Some(-14.0), // B
            22.0,
            &c,
        );
        // avg should be between a and b
        assert!(result.dic_avg >= result.dic_a.min(result.dic_b), "avg should be >= min(a,b)");
        assert!(result.dic_avg <= result.dic_a.max(result.dic_b), "avg should be <= max(a,b)");
        // std should be positive and finite
        assert!(result.dic_std > 0.0 && result.dic_std.is_finite(), "std should be positive, got {}", result.dic_std);
        // d13c should also be present
        assert!(result.d13c_avg.is_some());
        assert!(result.d13c_std.is_some());
        assert!(result.d13c_a.is_some());
        assert!(result.d13c_b.is_some());
    }
}
