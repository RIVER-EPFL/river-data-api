use serde::{Deserialize, Serialize};

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
}
