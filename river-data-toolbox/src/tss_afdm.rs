/// TSS (Total Suspended Solids) in mg/L.
///
/// From R `calcTSS`:
///   TSS = 1_000_000 * (wgt_dried - wgt_prefilt) / vol_filtered
///
/// Weights in grams, volume in mL. Factor converts g/mL to mg/L.
/// Returns NaN if vol_filtered is zero.
#[must_use]
pub fn tss_mg_l(wgt_dried_g: f64, wgt_prefilt_g: f64, vol_filtered_ml: f64) -> f64 {
    if vol_filtered_ml == 0.0 {
        return f64::NAN;
    }
    1_000_000.0 * (wgt_dried_g - wgt_prefilt_g) / vol_filtered_ml
}

/// AFDM (Ash-Free Dry Mass) in mg/L.
///
/// From R `calcAFDM`:
///   AFDM = 1_000_000 * (wgt_dried - wgt_ashed) / vol_filtered
///
/// Weights in grams, volume in mL.
/// Returns NaN if vol_filtered is zero.
#[must_use]
pub fn afdm_mg_l(wgt_dried_g: f64, wgt_ashed_g: f64, vol_filtered_ml: f64) -> f64 {
    if vol_filtered_ml == 0.0 {
        return f64::NAN;
    }
    1_000_000.0 * (wgt_dried_g - wgt_ashed_g) / vol_filtered_ml
}

/// Percent organic matter = AFDM / TSS * 100.
///
/// Returns NaN if TSS is zero.
#[must_use]
pub fn percent_organic(tss: f64, afdm: f64) -> f64 {
    if tss == 0.0 {
        return f64::NAN;
    }
    afdm / tss * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-3;

    #[test]
    fn test_tss_basic() {
        // dried=0.1050, prefilt=0.1000, vol=500 => 1e6*(0.005)/500 = 10.0 mg/L
        let result = tss_mg_l(0.1050, 0.1000, 500.0);
        assert!((result - 10.0).abs() < TOL, "expected 10.0, got {result}");
    }

    #[test]
    fn test_tss_zero_volume() {
        assert!(tss_mg_l(0.1050, 0.1000, 0.0).is_nan());
    }

    #[test]
    fn test_afdm_basic() {
        // dried=0.1050, ashed=0.1010, vol=500 => 1e6*(0.004)/500 = 8.0 mg/L
        let result = afdm_mg_l(0.1050, 0.1010, 500.0);
        assert!((result - 8.0).abs() < TOL, "expected 8.0, got {result}");
    }

    #[test]
    fn test_percent_organic() {
        // TSS=10, AFDM=8 => 80%
        let result = percent_organic(10.0, 8.0);
        assert!((result - 80.0).abs() < TOL, "expected 80.0, got {result}");
    }

    #[test]
    fn test_percent_organic_zero_tss() {
        assert!(percent_organic(0.0, 5.0).is_nan());
    }

    #[test]
    fn test_negative_tss() {
        // prefilt > dried => negative TSS (possible with measurement error)
        let result = tss_mg_l(0.0990, 0.1000, 500.0);
        assert!(result < 0.0, "expected negative TSS, got {result}");
    }
}
