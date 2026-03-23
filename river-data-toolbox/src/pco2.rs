use serde::{Deserialize, Serialize};

// ============================================================================
// Constants
// ============================================================================

/// Physical constants for gas calculations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GasConstants {
    /// Henry's law constant for CO2 at 298.15K (mol/(L·atm)).
    /// CNET default: `h_co2_29815k` ≈ 0.034
    pub kh_co2: f64,
    /// Temperature dependence constant for CO2 Henry's law.
    /// CNET `c_const` ≈ 2392.86
    pub c_const: f64,
    /// Universal gas constant (L·atm/(mol·K)).
    /// CNET `gas_const_r_atm` ≈ 0.08206
    pub gas_const_r_atm: f64,
    /// Universal gas constant (J/(mol·K)).
    /// CNET `gas_const_r_mol` ≈ 8.314
    pub gas_const_r_mol: f64,
    /// Henry's law constant for CH4 at 298.15K (mol/(L·atm)).
    /// CNET `h_ch4_29815k`
    pub kh_ch4: f64,
    /// CH4 temperature dependence constant.
    /// CNET: 1750
    pub ch4_temp_const: f64,
    /// CH4 concentration in standard atmosphere (ppm).
    /// CNET `ch4_in_sa`
    pub ch4_in_sa: f64,
}

impl Default for GasConstants {
    fn default() -> Self {
        Self {
            kh_co2: 0.034,
            c_const: 2392.86,
            gas_const_r_atm: 0.082_06,
            gas_const_r_mol: 8.314,
            kh_ch4: 0.001_4,
            ch4_temp_const: 1750.0,
            ch4_in_sa: 1.9,
        }
    }
}

/// Result of a pCO2 calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PCO2Result {
    /// CO2 aqueous concentration in µM.
    pub co2_aq_umol: f64,
    /// pCO2 in µatm.
    pub pco2_uatm: f64,
}

/// CH4 dry concentration corrected for water vapor.
///
/// From R `calcCH4dry`:
///   ch4_dry = (h2o * 1.2347 - 0.0016) * ch4 / 100 + ch4
#[must_use]
pub fn ch4_dry(ch4_raw: f64, h2o_percent: f64) -> f64 {
    (h2o_percent * 1.2347 - 0.0016) * ch4_raw / 100.0 + ch4_raw
}

/// pCO2 from headspace CO2aq concentration (µM) — simplest variant.
///
/// From R `calcpCO2`:
///   pCO2 = CO2_aq / (kh_co2 * exp(c_const * (1/T_water - 1/298.15)))
///
/// CO2 aqueous is passed directly; pCO2 is derived from Henry's law.
#[must_use]
pub fn pco2_from_co2aq(co2_aq_umol: f64, water_temp_c: f64, constants: &GasConstants) -> f64 {
    let t_water_k = water_temp_c + 273.15;
    let kh_t = constants.kh_co2 * (constants.c_const * (1.0 / t_water_k - 1.0 / 298.15)).exp();
    if kh_t == 0.0 {
        return f64::NAN;
    }
    co2_aq_umol / kh_t
}

/// pCO2 variant P1: pressure-corrected with barometric pressure.
///
/// From R `calcpCO2P1`:
///   pCO2 = CO2_aq * bp / (kh_co2 * exp(c_const * (1/T - 1/298.15)) * 1013.25)
#[must_use]
pub fn pco2_p1(
    co2_aq_umol: f64,
    water_temp_c: f64,
    pressure_hpa: f64,
    constants: &GasConstants,
) -> f64 {
    let t_water_k = water_temp_c + 273.15;
    let kh_t = constants.kh_co2 * (constants.c_const * (1.0 / t_water_k - 1.0 / 298.15)).exp();
    let divisor = kh_t * 1013.25;
    if divisor == 0.0 {
        return f64::NAN;
    }
    co2_aq_umol * pressure_hpa / divisor
}

/// pCO2 variant P2: inverse pressure correction.
///
/// From R `calcpCO2P2`:
///   pCO2 = CO2_aq * 1013.25 / (kh_co2 * exp(c_const * (1/T - 1/298.15)) * bp)
#[must_use]
pub fn pco2_p2(
    co2_aq_umol: f64,
    water_temp_c: f64,
    pressure_hpa: f64,
    constants: &GasConstants,
) -> f64 {
    let t_water_k = water_temp_c + 273.15;
    let kh_t = constants.kh_co2 * (constants.c_const * (1.0 / t_water_k - 1.0 / 298.15)).exp();
    let divisor = kh_t * pressure_hpa;
    if divisor == 0.0 {
        return f64::NAN;
    }
    co2_aq_umol * 1013.25 / divisor
}

/// Dissolved CH4 from headspace analysis.
///
/// From R `calcCH4`:
///   Uses Henry's law for CH4 with lab temperature/pressure corrections.
///   Returns CH4 in µmol/L.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn dissolved_ch4(
    ch4_dry_ppm: f64,
    water_temp_c: f64,
    pressure_hpa: f64,
    lab_temp_c: f64,
    lab_pressure_atm: f64,
    constants: &GasConstants,
) -> f64 {
    let t_water_k = water_temp_c + 273.15;
    let t_lab_k = lab_temp_c + 273.15;
    let bp = pressure_hpa;

    let h_ch4_t_eq = constants.kh_ch4
        * (constants.ch4_temp_const * (1.0 / t_lab_k - 1.0 / 298.15)).exp();

    let a = ch4_dry_ppm * (lab_pressure_atm * 1013.25) * 101.325 * t_water_k
        - bp * (constants.ch4_in_sa * t_lab_k * 1e3);
    let b = h_ch4_t_eq * constants.gas_const_r_mol * 10.0 * t_water_k + bp;

    let dividend = a * b;
    let divisor = t_lab_k * bp * constants.gas_const_r_mol * t_water_k;

    if divisor == 0.0 {
        return f64::NAN;
    }
    dividend / divisor
}

// ============================================================================
// Full pipeline (raw Picarro → all derived values)
// ============================================================================

/// Input for the full pCO2 pipeline starting from raw Picarro data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pco2FullInput {
    pub co2_ppm: f64,
    pub h2o_percent: f64,
    pub ch4_ppm: f64,
    pub d13co2_permil: Option<f64>,
    pub lab_temp_c: f64,
    pub lab_pressure_atm: f64,
    pub vol_sa_ml: f64,
    pub vol_water_ml: f64,
    pub water_temp_c: f64,
    pub field_pressure_hpa: f64,
}

/// All outputs from the full pCO2 pipeline, matching legacy CNET naming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pco2FullResult {
    /// CO2 headspace in µmol/L.
    pub co2_hs_umol: f64,
    /// pCO2 simple (µatm).
    pub pco2_uatm: f64,
    /// pCO2 P1 (µatm).
    pub pco2_p1_uatm: f64,
    /// pCO2 P2 (µatm).
    pub pco2_p2_uatm: f64,
    /// CH4 dry (ppm).
    pub ch4_dry_ppm: f64,
    /// Dissolved CH4 (µmol/L).
    pub ch4_dissolved_umol: f64,
    /// δ13C-CO2 pass-through (‰).
    pub d13co2_permil: Option<f64>,
}

/// Run the full pCO2 pipeline from raw Picarro data.
///
/// 1. CO2 headspace from raw ppm via `co2_headspace()`
/// 2. pCO2 simple, P1, P2 from headspace CO2aq
/// 3. CH4 dry correction
/// 4. Dissolved CH4
/// 5. δ13C-CO2 pass-through
#[must_use]
pub fn pco2_full_pipeline(input: &Pco2FullInput, constants: &GasConstants) -> Pco2FullResult {
    // 1. CO2 headspace (µmol/L)
    let co2_hs_umol = crate::co2_air::co2_headspace(
        input.co2_ppm,
        input.lab_temp_c,
        input.lab_pressure_atm,
        input.vol_sa_ml,
        input.vol_water_ml,
        constants,
    );

    // 2. pCO2 variants
    let pco2_uatm = pco2_from_co2aq(co2_hs_umol, input.water_temp_c, constants);
    let pco2_p1_uatm = pco2_p1(co2_hs_umol, input.water_temp_c, input.field_pressure_hpa, constants);
    let pco2_p2_uatm = pco2_p2(co2_hs_umol, input.water_temp_c, input.field_pressure_hpa, constants);

    // 3. CH4 dry
    let ch4_dry_ppm = ch4_dry(input.ch4_ppm, input.h2o_percent);

    // 4. Dissolved CH4
    let ch4_dissolved_umol = dissolved_ch4(
        ch4_dry_ppm,
        input.water_temp_c,
        input.field_pressure_hpa,
        input.lab_temp_c,
        input.lab_pressure_atm,
        constants,
    );

    Pco2FullResult {
        co2_hs_umol,
        pco2_uatm,
        pco2_p1_uatm,
        pco2_p2_uatm,
        ch4_dry_ppm,
        ch4_dissolved_umol,
        d13co2_permil: input.d13co2_permil,
    }
}

// ============================================================================
// Replicate averaging
// ============================================================================

/// Averaged results from two replicates (A and B) of the full pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pco2ReplicateResult {
    pub a: Pco2FullResult,
    pub b: Pco2FullResult,
    // Averages
    pub co2_hs_umol_avg: f64,
    pub pco2_uatm_avg: f64,
    pub pco2_p1_uatm_avg: f64,
    pub pco2_p2_uatm_avg: f64,
    pub ch4_dry_ppm_avg: f64,
    pub ch4_dissolved_umol_avg: f64,
    pub d13co2_permil_avg: Option<f64>,
    // Sample standard deviations
    pub co2_hs_umol_sd: f64,
    pub pco2_uatm_sd: f64,
    pub pco2_p1_uatm_sd: f64,
    pub pco2_p2_uatm_sd: f64,
    pub ch4_dry_ppm_sd: f64,
    pub ch4_dissolved_umol_sd: f64,
    pub d13co2_permil_sd: Option<f64>,
}

/// Sample standard deviation for two values: |a - b| / sqrt(2).
fn sd2(a: f64, b: f64) -> f64 {
    (a - b).abs() / 2.0_f64.sqrt()
}

/// Run the full pipeline on two replicates and return averages + SDs.
#[must_use]
pub fn pco2_replicates(
    input_a: &Pco2FullInput,
    input_b: &Pco2FullInput,
    constants: &GasConstants,
) -> Pco2ReplicateResult {
    let a = pco2_full_pipeline(input_a, constants);
    let b = pco2_full_pipeline(input_b, constants);

    let d13_avg = match (a.d13co2_permil, b.d13co2_permil) {
        (Some(da), Some(db)) => Some((da + db) / 2.0),
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    };
    let d13_sd = match (a.d13co2_permil, b.d13co2_permil) {
        (Some(da), Some(db)) => Some(sd2(da, db)),
        _ => None,
    };

    Pco2ReplicateResult {
        co2_hs_umol_avg: (a.co2_hs_umol + b.co2_hs_umol) / 2.0,
        pco2_uatm_avg: (a.pco2_uatm + b.pco2_uatm) / 2.0,
        pco2_p1_uatm_avg: (a.pco2_p1_uatm + b.pco2_p1_uatm) / 2.0,
        pco2_p2_uatm_avg: (a.pco2_p2_uatm + b.pco2_p2_uatm) / 2.0,
        ch4_dry_ppm_avg: (a.ch4_dry_ppm + b.ch4_dry_ppm) / 2.0,
        ch4_dissolved_umol_avg: (a.ch4_dissolved_umol + b.ch4_dissolved_umol) / 2.0,
        d13co2_permil_avg: d13_avg,
        co2_hs_umol_sd: sd2(a.co2_hs_umol, b.co2_hs_umol),
        pco2_uatm_sd: sd2(a.pco2_uatm, b.pco2_uatm),
        pco2_p1_uatm_sd: sd2(a.pco2_p1_uatm, b.pco2_p1_uatm),
        pco2_p2_uatm_sd: sd2(a.pco2_p2_uatm, b.pco2_p2_uatm),
        ch4_dry_ppm_sd: sd2(a.ch4_dry_ppm, b.ch4_dry_ppm),
        ch4_dissolved_umol_sd: sd2(a.ch4_dissolved_umol, b.ch4_dissolved_umol),
        d13co2_permil_sd: d13_sd,
        a,
        b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 0.01;

    #[test]
    fn test_ch4_dry() {
        // From R: (h2o * 1.2347 - 0.0016) * ch4 / 100 + ch4
        // h2o=1.5, ch4=2000 => (1.5*1.2347 - 0.0016)*2000/100 + 2000
        // = (1.85205-0.0016)*20 + 2000 = 1.85045*20 + 2000 = 37.009 + 2000 = 2037.009
        let result = ch4_dry(2000.0, 1.5);
        let expected = (1.5 * 1.2347 - 0.0016) * 2000.0 / 100.0 + 2000.0;
        assert!(
            (result - expected).abs() < TOL,
            "expected {expected}, got {result}"
        );
    }

    #[test]
    fn test_pco2_from_co2aq() {
        let constants = GasConstants::default();
        // At 15°C, CO2aq = 50 µM
        let result = pco2_from_co2aq(50.0, 15.0, &constants);
        // kh_t = 0.034 * exp(2392.86 * (1/288.15 - 1/298.15))
        // Should give a finite positive value
        assert!(result > 0.0 && result.is_finite(), "expected positive pCO2, got {result}");
    }

    #[test]
    fn test_pco2_p1_vs_p2_reciprocal() {
        let constants = GasConstants::default();
        // P1 and P2 should be reciprocals in pressure: P1*P2 = CO2^2 * 1013.25 / (kh^2)
        let co2 = 50.0;
        let temp = 15.0;
        let bp = 900.0;
        let p1 = pco2_p1(co2, temp, bp, &constants);
        let p2 = pco2_p2(co2, temp, bp, &constants);
        // P1/P2 should equal bp^2 / 1013.25^2
        let ratio = p1 / p2;
        let expected_ratio = (bp / 1013.25).powi(2);
        assert!(
            (ratio - expected_ratio).abs() < 0.001,
            "P1/P2 ratio {ratio} != expected {expected_ratio}"
        );
    }

    fn make_test_input(co2_ppm: f64, ch4_ppm: f64, d13: Option<f64>) -> Pco2FullInput {
        Pco2FullInput {
            co2_ppm,
            h2o_percent: 1.5,
            ch4_ppm,
            d13co2_permil: d13,
            lab_temp_c: 22.0,
            lab_pressure_atm: 0.95,
            vol_sa_ml: 60.0,
            vol_water_ml: 40.0,
            water_temp_c: 12.0,
            field_pressure_hpa: 960.0,
        }
    }

    #[test]
    fn test_full_pipeline_results_finite_and_positive() {
        let constants = GasConstants::default();
        let input = make_test_input(3000.0, 5.0, Some(-12.5));
        let result = pco2_full_pipeline(&input, &constants);

        assert!(result.co2_hs_umol > 0.0 && result.co2_hs_umol.is_finite(),
            "co2_hs_umol should be positive and finite, got {}", result.co2_hs_umol);
        assert!(result.pco2_uatm > 0.0 && result.pco2_uatm.is_finite(),
            "pco2_uatm should be positive and finite, got {}", result.pco2_uatm);
        assert!(result.pco2_p1_uatm > 0.0 && result.pco2_p1_uatm.is_finite(),
            "pco2_p1_uatm should be positive and finite, got {}", result.pco2_p1_uatm);
        assert!(result.pco2_p2_uatm > 0.0 && result.pco2_p2_uatm.is_finite(),
            "pco2_p2_uatm should be positive and finite, got {}", result.pco2_p2_uatm);
        assert!(result.ch4_dry_ppm > 0.0 && result.ch4_dry_ppm.is_finite(),
            "ch4_dry_ppm should be positive and finite, got {}", result.ch4_dry_ppm);
        assert!(result.ch4_dissolved_umol.is_finite(),
            "ch4_dissolved_umol should be finite, got {}", result.ch4_dissolved_umol);
        assert_eq!(result.d13co2_permil, Some(-12.5));
    }

    #[test]
    fn test_full_pipeline_co2hs_feeds_pco2() {
        // Verify the pipeline correctly chains co2_headspace → pco2_from_co2aq
        let constants = GasConstants::default();
        let input = make_test_input(3000.0, 5.0, None);
        let result = pco2_full_pipeline(&input, &constants);

        // Manually compute what pco2_from_co2aq should give for the same co2_hs
        let expected_pco2 = pco2_from_co2aq(result.co2_hs_umol, input.water_temp_c, &constants);
        assert!(
            (result.pco2_uatm - expected_pco2).abs() < 1e-10,
            "pipeline pco2 {} != direct pco2 {}", result.pco2_uatm, expected_pco2
        );
    }

    #[test]
    fn test_replicates_averages_and_sds_finite() {
        let constants = GasConstants::default();
        let a = make_test_input(3000.0, 5.0, Some(-12.0));
        let b = make_test_input(3200.0, 5.5, Some(-13.0));
        let rep = pco2_replicates(&a, &b, &constants);

        assert!(rep.co2_hs_umol_avg.is_finite(), "avg should be finite");
        assert!(rep.pco2_uatm_avg.is_finite(), "avg should be finite");
        assert!(rep.pco2_p1_uatm_avg.is_finite(), "avg should be finite");
        assert!(rep.pco2_p2_uatm_avg.is_finite(), "avg should be finite");
        assert!(rep.ch4_dry_ppm_avg.is_finite(), "avg should be finite");
        assert!(rep.ch4_dissolved_umol_avg.is_finite(), "avg should be finite");

        assert!(rep.co2_hs_umol_sd >= 0.0 && rep.co2_hs_umol_sd.is_finite());
        assert!(rep.pco2_uatm_sd >= 0.0 && rep.pco2_uatm_sd.is_finite());
        assert!(rep.pco2_p1_uatm_sd >= 0.0 && rep.pco2_p1_uatm_sd.is_finite());
        assert!(rep.pco2_p2_uatm_sd >= 0.0 && rep.pco2_p2_uatm_sd.is_finite());
        assert!(rep.ch4_dry_ppm_sd >= 0.0 && rep.ch4_dry_ppm_sd.is_finite());
        assert!(rep.ch4_dissolved_umol_sd >= 0.0 && rep.ch4_dissolved_umol_sd.is_finite());

        assert!(rep.d13co2_permil_avg.is_some());
        assert!(rep.d13co2_permil_sd.is_some());
    }

    #[test]
    fn test_replicates_sd_formula() {
        // Verify SD for two values matches |a - b| / sqrt(2)
        let constants = GasConstants::default();
        let a = make_test_input(3000.0, 5.0, None);
        let b = make_test_input(3200.0, 5.5, None);
        let rep = pco2_replicates(&a, &b, &constants);

        let expected_sd = (rep.a.co2_hs_umol - rep.b.co2_hs_umol).abs() / 2.0_f64.sqrt();
        assert!(
            (rep.co2_hs_umol_sd - expected_sd).abs() < 1e-10,
            "SD {} != expected {}", rep.co2_hs_umol_sd, expected_sd
        );
    }

    #[test]
    fn test_replicates_identical_inputs_zero_sd() {
        let constants = GasConstants::default();
        let input = make_test_input(3000.0, 5.0, Some(-12.0));
        let rep = pco2_replicates(&input, &input, &constants);

        assert!((rep.co2_hs_umol_sd).abs() < 1e-10, "identical inputs should give SD=0");
        assert!((rep.pco2_uatm_sd).abs() < 1e-10, "identical inputs should give SD=0");
        assert_eq!(rep.d13co2_permil_sd, Some(0.0));
    }
}
