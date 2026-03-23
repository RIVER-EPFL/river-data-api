use serde::{Deserialize, Serialize};

use crate::benthic;
use crate::common;

/// Chlorophyll-a with acid correction method.
///
/// From R `calcChlaAcid`:
///   chla = (fluor_before - fluor_after) * slope + intercept
///
/// `fluor_before`: fluorescence reading before acidification
/// `fluor_after`: fluorescence reading after acidification
/// `slope`, `intercept`: from standard curve
#[must_use]
pub fn chla_acid(fluor_before: f64, fluor_after: f64, slope: f64, intercept: f64) -> f64 {
    (fluor_before - fluor_after) * slope + intercept
}

/// Chlorophyll-a without acid (direct fluorescence).
///
/// From R `calcChlaNoAcid`:
///   chla = fluorescence * slope + intercept
#[must_use]
pub fn chla_no_acid(fluorescence: f64, slope: f64, intercept: f64) -> f64 {
    fluorescence * slope + intercept
}

// ============================================================================
// Unified Chlorophyll-Benthic multi-replicate processing
// ============================================================================

/// Input for a single replicate in the unified Chla-Benthic tool.
#[derive(Debug, Clone)]
pub struct ChlaReplicateInput {
    pub fluor_before: f64,
    /// `None` for no-acid only mode
    pub fluor_after: Option<f64>,
    pub vol_total_ml: f64,
    /// vol_filtered = vol_total - vol_after
    pub vol_after_ml: f64,
    /// Typically 3 rock dimensions (cm)
    pub diameters_cm: Vec<f64>,
    /// Optional AFDM weight (g per filter)
    pub afdm_g_filter: Option<f64>,
}

/// Output for a single replicate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChlaReplicateOutput {
    pub vol_filtered_ml: f64,
    pub chla_acid_ug_l: Option<f64>,
    pub chla_noacid_ug_l: f64,
    pub rock_area_m2: f64,
    pub chla_acid_ug_m2: Option<f64>,
    pub chla_noacid_ug_m2: f64,
    pub afdm_g_m2: Option<f64>,
}

/// Aggregated result across all replicates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChlaBenthicResult {
    pub replicates: Vec<ChlaReplicateOutput>,
    pub chla_acid_ug_l_avg: Option<f64>,
    pub chla_acid_ug_l_sd: Option<f64>,
    pub chla_noacid_ug_l_avg: f64,
    pub chla_noacid_ug_l_sd: f64,
    pub chla_acid_ug_m2_avg: Option<f64>,
    pub chla_acid_ug_m2_sd: Option<f64>,
    pub chla_noacid_ug_m2_avg: f64,
    pub chla_noacid_ug_m2_sd: f64,
    pub afdm_g_m2_avg: Option<f64>,
    pub afdm_g_m2_sd: Option<f64>,
}

/// Process multiple chlorophyll-benthic replicates (up to 5, matching the legacy
/// CNET/METALP portal). Computes acid and no-acid Chl-a, per-m2 normalizations,
/// benthic AFDM per m2, and cross-replicate averages/SDs.
#[must_use]
pub fn chla_benthic_replicates(
    inputs: &[ChlaReplicateInput],
    acid_slope: f64,
    acid_intercept: f64,
    noacid_slope: f64,
    noacid_intercept: f64,
) -> ChlaBenthicResult {
    let replicates: Vec<ChlaReplicateOutput> = inputs
        .iter()
        .map(|inp| {
            let vol_filtered = inp.vol_total_ml - inp.vol_after_ml;

            let chla_acid_val = inp
                .fluor_after
                .map(|after| chla_acid(inp.fluor_before, after, acid_slope, acid_intercept));

            let chla_noacid_val = chla_no_acid(inp.fluor_before, noacid_slope, noacid_intercept);

            let rock_area = benthic::rock_surface_area_m2(&inp.diameters_cm);

            let chla_acid_m2 = chla_acid_val.map(|chla| {
                benthic::per_m2(chla * 0.005, inp.vol_total_ml, vol_filtered, rock_area)
            });

            let chla_noacid_m2 =
                benthic::per_m2(chla_noacid_val * 0.005, inp.vol_total_ml, vol_filtered, rock_area);

            let afdm_m2 = inp.afdm_g_filter.map(|afdm| {
                benthic::per_m2(afdm, inp.vol_total_ml, vol_filtered, rock_area)
            });

            ChlaReplicateOutput {
                vol_filtered_ml: vol_filtered,
                chla_acid_ug_l: chla_acid_val,
                chla_noacid_ug_l: chla_noacid_val,
                rock_area_m2: rock_area,
                chla_acid_ug_m2: chla_acid_m2,
                chla_noacid_ug_m2: chla_noacid_m2,
                afdm_g_m2: afdm_m2,
            }
        })
        .collect();

    // Collect values for cross-replicate statistics
    let acid_ug_l: Vec<f64> = replicates.iter().filter_map(|r| r.chla_acid_ug_l).collect();
    let noacid_ug_l: Vec<f64> = replicates.iter().map(|r| r.chla_noacid_ug_l).collect();
    let acid_ug_m2: Vec<f64> = replicates.iter().filter_map(|r| r.chla_acid_ug_m2).collect();
    let noacid_ug_m2: Vec<f64> = replicates.iter().map(|r| r.chla_noacid_ug_m2).collect();
    let afdm_vals: Vec<f64> = replicates.iter().filter_map(|r| r.afdm_g_m2).collect();

    let has_acid = !acid_ug_l.is_empty();
    let has_afdm = !afdm_vals.is_empty();

    ChlaBenthicResult {
        replicates,
        chla_acid_ug_l_avg: if has_acid { Some(common::mean(&acid_ug_l)) } else { None },
        chla_acid_ug_l_sd: if has_acid { Some(common::std_dev(&acid_ug_l)) } else { None },
        chla_noacid_ug_l_avg: common::mean(&noacid_ug_l),
        chla_noacid_ug_l_sd: common::std_dev(&noacid_ug_l),
        chla_acid_ug_m2_avg: if has_acid { Some(common::mean(&acid_ug_m2)) } else { None },
        chla_acid_ug_m2_sd: if has_acid { Some(common::std_dev(&acid_ug_m2)) } else { None },
        chla_noacid_ug_m2_avg: common::mean(&noacid_ug_m2),
        chla_noacid_ug_m2_sd: common::std_dev(&noacid_ug_m2),
        afdm_g_m2_avg: if has_afdm { Some(common::mean(&afdm_vals)) } else { None },
        afdm_g_m2_sd: if has_afdm { Some(common::std_dev(&afdm_vals)) } else { None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-6;

    #[test]
    fn test_chla_acid() {
        // before=100, after=30, slope=0.5, intercept=1.0
        // (100-30)*0.5 + 1.0 = 36.0
        let result = chla_acid(100.0, 30.0, 0.5, 1.0);
        assert!((result - 36.0).abs() < TOL, "expected 36.0, got {result}");
    }

    #[test]
    fn test_chla_no_acid() {
        // fluor=50, slope=0.8, intercept=2.0 => 50*0.8+2.0 = 42.0
        let result = chla_no_acid(50.0, 0.8, 2.0);
        assert!((result - 42.0).abs() < TOL, "expected 42.0, got {result}");
    }

    #[test]
    fn test_chla_acid_zero_diff() {
        // before == after => only intercept
        let result = chla_acid(50.0, 50.0, 0.5, 1.0);
        assert!((result - 1.0).abs() < TOL, "expected 1.0, got {result}");
    }

    #[test]
    fn test_chla_negative_result() {
        // Possible if fluorescence is very low and intercept is negative
        let result = chla_no_acid(1.0, 0.5, -10.0);
        assert!(result < 0.0, "expected negative, got {result}");
    }

    #[test]
    fn test_chla_benthic_replicates_basic() {
        let inputs = vec![
            ChlaReplicateInput {
                fluor_before: 100.0,
                fluor_after: Some(30.0),
                vol_total_ml: 100.0,
                vol_after_ml: 50.0, // vol_filtered = 50
                diameters_cm: vec![10.0, 10.0, 10.0],
                afdm_g_filter: Some(0.005),
            },
            ChlaReplicateInput {
                fluor_before: 110.0,
                fluor_after: Some(35.0),
                vol_total_ml: 100.0,
                vol_after_ml: 50.0,
                diameters_cm: vec![10.0, 10.0, 10.0],
                afdm_g_filter: Some(0.006),
            },
        ];

        let result = chla_benthic_replicates(&inputs, 0.5, 1.0, 0.8, 2.0);

        assert_eq!(result.replicates.len(), 2);
        // vol_filtered = 100 - 50 = 50
        assert!((result.replicates[0].vol_filtered_ml - 50.0).abs() < TOL);
        // chla_acid = (100-30)*0.5+1 = 36
        assert!((result.replicates[0].chla_acid_ug_l.unwrap() - 36.0).abs() < TOL);
        // chla_noacid = 100*0.8+2 = 82
        assert!((result.replicates[0].chla_noacid_ug_l - 82.0).abs() < TOL);
        // Averages should exist
        assert!(result.chla_acid_ug_l_avg.is_some());
        assert!(result.chla_noacid_ug_l_avg.is_finite());
        assert!(result.afdm_g_m2_avg.is_some());
    }

    #[test]
    fn test_chla_benthic_no_acid_only() {
        let inputs = vec![
            ChlaReplicateInput {
                fluor_before: 50.0,
                fluor_after: None,
                vol_total_ml: 100.0,
                vol_after_ml: 40.0,
                diameters_cm: vec![8.0, 8.0, 8.0],
                afdm_g_filter: None,
            },
        ];

        let result = chla_benthic_replicates(&inputs, 0.5, 1.0, 0.8, 2.0);

        assert_eq!(result.replicates.len(), 1);
        assert!(result.replicates[0].chla_acid_ug_l.is_none());
        assert!(result.replicates[0].chla_acid_ug_m2.is_none());
        assert!(result.replicates[0].afdm_g_m2.is_none());
        // No acid values => averages should be None
        assert!(result.chla_acid_ug_l_avg.is_none());
        assert!(result.afdm_g_m2_avg.is_none());
        // noacid = 50*0.8+2 = 42
        assert!((result.replicates[0].chla_noacid_ug_l - 42.0).abs() < TOL);
    }
}
