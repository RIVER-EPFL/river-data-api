use std::f64::consts::PI;

/// Rock surface area from diameter measurements using the Knud Thomsen approximation
/// for a triaxial ellipsoid.
///
/// From R `convertToUnitPerM2`:
///   area = 2π × mean(combn((d/100)^1.6075, 2, prod))^(1/1.6075)
///
/// Takes three diameter measurements (cm) representing the three semi-axes
/// of the rock ellipsoid. Converts to meters internally.
///
/// Returns surface area in m².
#[must_use]
pub fn rock_surface_area_m2(diameters_cm: &[f64]) -> f64 {
    if diameters_cm.len() < 2 {
        return f64::NAN;
    }

    let p = 1.6075;

    // Convert cm to m and raise to power p
    let d_p: Vec<f64> = diameters_cm.iter().map(|d| (d / 100.0).powf(p)).collect();

    // Generate all 2-combinations and compute their products
    let mut products = Vec::new();
    for i in 0..d_p.len() {
        for j in (i + 1)..d_p.len() {
            products.push(d_p[i] * d_p[j]);
        }
    }

    if products.is_empty() {
        return f64::NAN;
    }

    let mean_prod: f64 = products.iter().sum::<f64>() / products.len() as f64;

    2.0 * PI * mean_prod.powf(1.0 / p)
}

/// Convert a per-volume measurement to per-area (per m²) using rock surface area.
///
/// From R `convertToUnitPerM2`:
///   result = sample_value * total_volume / (volume_filtered * surface_area)
///
/// `value`: the measured value (e.g., AFDM in g per filter, or Chl-a in µg/L)
/// `total_volume_ml`: total extraction/sample volume
/// `volume_filtered_ml`: volume of water filtered through
/// `surface_area_m2`: rock surface area from `rock_surface_area_m2`
#[must_use]
pub fn per_m2(
    value: f64,
    total_volume_ml: f64,
    volume_filtered_ml: f64,
    surface_area_m2: f64,
) -> f64 {
    if volume_filtered_ml == 0.0 || surface_area_m2 == 0.0 {
        return f64::NAN;
    }
    value * total_volume_ml / (volume_filtered_ml * surface_area_m2)
}

/// Benthic AFDM per m² from rock scrub sample.
///
/// From R `calcBenthicAFDM`:
///   Combines AFDM with rock dimensions and extraction volumes.
#[must_use]
pub fn benthic_afdm_per_m2(
    afdm_g_filter: f64,
    diameters_cm: &[f64],
    volume_filtered_ml: f64,
    total_volume_ml: f64,
) -> f64 {
    let area = rock_surface_area_m2(diameters_cm);
    per_m2(afdm_g_filter, total_volume_ml, volume_filtered_ml, area)
}

/// Benthic Chl-a per m² from rock scrub sample.
///
/// From R `calcChlaPerM2`:
///   chla_per_m2 = convertToUnitPerM2(chla_ugl * 0.005, diameters, vol_filtered, tot_vol)
///
/// The 0.005 factor converts µg/L to mg per extraction volume (5mL standard).
#[must_use]
pub fn benthic_chla_per_m2(
    chla_ug_l: f64,
    diameters_cm: &[f64],
    volume_filtered_ml: f64,
    total_volume_ml: f64,
) -> f64 {
    let area = rock_surface_area_m2(diameters_cm);
    per_m2(chla_ug_l * 0.005, total_volume_ml, volume_filtered_ml, area)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-6;

    #[test]
    fn test_rock_surface_area_sphere() {
        // The R formula: 2π × mean(combn((d/100)^p, 2, prod))^(1/p)
        // For d=[10,10,10]: 2π × (0.1^(2p))^(1/p) = 2π × 0.1² = 2π × 0.01
        // This computes the half-ellipsoid (exposed rock face) area.
        let area = rock_surface_area_m2(&[10.0, 10.0, 10.0]);
        let expected = 2.0 * PI * 0.1_f64.powi(2);
        assert!(
            (area - expected).abs() < 0.001,
            "expected ~{expected:.6}, got {area:.6}"
        );
    }

    #[test]
    fn test_rock_surface_area_ellipsoid() {
        // Non-spherical: 10cm × 8cm × 6cm
        let area = rock_surface_area_m2(&[10.0, 8.0, 6.0]);
        assert!(
            area > 0.0 && area.is_finite(),
            "expected positive area, got {area}"
        );
        // Should be between the half-ellipsoid bounds for d=6 and d=10
        let half_10 = 2.0 * PI * 0.1_f64.powi(2);
        let half_6 = 2.0 * PI * 0.06_f64.powi(2);
        assert!(
            area < half_10 && area > half_6,
            "area {area:.6} should be between {half_6:.6} and {half_10:.6}"
        );
    }

    #[test]
    fn test_rock_surface_area_insufficient_dims() {
        assert!(rock_surface_area_m2(&[10.0]).is_nan());
        assert!(rock_surface_area_m2(&[]).is_nan());
    }

    #[test]
    fn test_per_m2_basic() {
        // value=0.5, total=100mL, filtered=50mL, area=0.01m²
        // => 0.5 * 100 / (50 * 0.01) = 100
        let result = per_m2(0.5, 100.0, 50.0, 0.01);
        assert!((result - 100.0).abs() < TOL, "expected 100.0, got {result}");
    }

    #[test]
    fn test_per_m2_zero_area() {
        assert!(per_m2(0.5, 100.0, 50.0, 0.0).is_nan());
    }

    #[test]
    fn test_benthic_afdm() {
        let result = benthic_afdm_per_m2(0.005, &[10.0, 8.0, 6.0], 50.0, 100.0);
        assert!(
            result > 0.0 && result.is_finite(),
            "expected positive AFDM/m², got {result}"
        );
    }

    #[test]
    fn test_benthic_chla() {
        let result = benthic_chla_per_m2(15.0, &[10.0, 8.0, 6.0], 50.0, 100.0);
        assert!(
            result > 0.0 && result.is_finite(),
            "expected positive Chl-a/m², got {result}"
        );
    }
}
