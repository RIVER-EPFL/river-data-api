use crate::pco2::GasConstants;

/// Headspace CO2 concentration (µmol/L) from Picarro analysis.
///
/// From R `calcCO2`:
///   exponent = exp(c_const * (1/T_lab_K - 1/298.15))
///   CO2 = co2_ppm * P_lab * (vol_sa + kh_co2 * exponent * vol_water * R_atm * T_lab_K) / (R_atm * vol_water * T_lab_K)
///
/// Returns CO2 concentration in µmol/L.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn co2_headspace(
    co2_ppm: f64,
    lab_temp_c: f64,
    lab_pressure_atm: f64,
    vol_sa_ml: f64,
    vol_water_ml: f64,
    constants: &GasConstants,
) -> f64 {
    let t_lab_k = lab_temp_c + 273.15;
    let exponent = (constants.c_const * (1.0 / t_lab_k - 1.0 / 298.15)).exp();
    let dividend = co2_ppm
        * lab_pressure_atm
        * (vol_sa_ml
            + constants.kh_co2 * exponent * vol_water_ml * constants.gas_const_r_atm * t_lab_k);
    let divisor = constants.gas_const_r_atm * vol_water_ml * t_lab_k;

    if divisor == 0.0 {
        return f64::NAN;
    }
    dividend / divisor
}

/// CO2 dry concentration from wet measurement, corrected for water vapor.
///
/// Simple dilution correction: CO2_dry = CO2_wet / (1 - h2o_fraction)
/// where h2o is in percent.
#[must_use]
pub fn co2_dry(co2_wet: f64, h2o_percent: f64) -> f64 {
    let h2o_fraction = h2o_percent / 100.0;
    if (1.0 - h2o_fraction) == 0.0 {
        return f64::NAN;
    }
    co2_wet / (1.0 - h2o_fraction)
}

/// CH4 dry concentration from wet measurement.
///
/// From R `calcCH4dry`:
///   ch4_dry = (h2o * 1.2347 - 0.0016) * ch4 / 100 + ch4
///
/// Re-exported from pco2 module but also available here for the air context.
#[must_use]
pub fn ch4_dry_air(ch4_wet: f64, h2o_percent: f64) -> f64 {
    crate::pco2::ch4_dry(ch4_wet, h2o_percent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_co2_headspace_positive() {
        let constants = GasConstants::default();
        // co2=400ppm, lab_temp=22°C, lab_pressure=0.95atm, vol_sa=60mL, vol_water=40mL
        let result = co2_headspace(400.0, 22.0, 0.95, 60.0, 40.0, &constants);
        assert!(
            result > 0.0 && result.is_finite(),
            "expected positive CO2, got {result}"
        );
    }

    #[test]
    fn test_co2_dry_correction() {
        // 400 ppm wet at 2% h2o => 400 / 0.98 ≈ 408.16
        let result = co2_dry(400.0, 2.0);
        assert!(
            (result - 408.163).abs() < 0.1,
            "expected ~408.16, got {result}"
        );
    }

    #[test]
    fn test_co2_dry_zero_h2o() {
        let result = co2_dry(400.0, 0.0);
        assert!((result - 400.0).abs() < 1e-10);
    }

    #[test]
    fn test_ch4_dry_air_matches_pco2() {
        let a = ch4_dry_air(2000.0, 1.5);
        let b = crate::pco2::ch4_dry(2000.0, 1.5);
        assert!((a - b).abs() < 1e-10);
    }
}
