//! Exact-value portal-parity tests for the analytical tools. Expected numbers come from the
//! verbatim CNET/METALP portal R functions, which are seeded into every tool script as
//! `migration/tool_seed/prelude.R` and run by the tools runner. The same numbers are pinned per
//! tool in `migration/tool_seed/{tool}/cases.json`, which is what a version has to reproduce
//! before it can be activated.

use serial_test::serial;

async fn setup() -> (axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db);
    (app, token)
}

const REL_TOL: f64 = 1e-9;

fn assert_value(json: &serde_json::Value, key: &str, expected: f64) {
    let actual = json["results"][key]
        .as_f64()
        .unwrap_or_else(|| panic!("{key} missing or non-numeric in {:?}", json["results"]));
    let bound = REL_TOL * expected.abs().max(1.0);
    assert!(
        (actual - expected).abs() <= bound,
        "{key}: expected {expected}, got {actual}"
    );
}

fn assert_absent(json: &serde_json::Value, key: &str) {
    assert!(
        json["results"].get(key).is_none(),
        "{key} should be omitted from results, got {:?}",
        json["results"]
    );
}

async fn calculate(
    app: &axum::Router,
    tool: &str,
    payload: serde_json::Value,
    token: &str,
) -> serde_json::Value {
    let (status, json) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tools/{tool}/calculate"),
        &payload,
        token,
    )
    .await;
    assert_eq!(status, 200, "{tool} calculate failed: {json:?}");
    assert_eq!(json["tool"], tool);
    json
}

#[tokio::test]
#[serial]
async fn test_list_tools_excludes_removed() {
    let (app, token) = setup().await;

    let (status, json) = crate::common::get_json_with_token(&app, "/api/tools", &token).await;
    assert_eq!(status, 200);

    let names: Vec<&str> = json
        .as_array()
        .expect("tools should be an array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    // Counting is not a property of this endpoint: the authoring suites create scripts in the
    // same database and cleanup does not remove them, so assert the seeded set is served.
    for expected in [
        "doc",
        "tss_afdm",
        "chlorophyll",
        "nutrients",
        "alkalinity",
        "pco2",
        "dic",
        "dom",
        "field_data",
        "co2_air",
        "benthic",
        "chla_benthic",
        "discharge",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }
    assert!(!names.contains(&"ions"));
    assert!(!names.contains(&"isotopes"));
}

#[tokio::test]
#[serial]
async fn test_removed_tools_return_404() {
    let (app, token) = setup().await;

    // Tools and helper calculations dropped for lacking a portal counterpart:
    // the ion charge balance, the isotope excesses, Gran titration alkalinity,
    // the standalone dry-CO2 correction, spectral slope, and percent organic.
    for tool in [
        "ions",
        "isotopes",
        "gran_titration",
        "co2_dry",
        "spectral_slope",
        "percent_organic",
    ] {
        let (status, _) = crate::common::post_json_parse_with_token(
            &app,
            &format!("/api/tools/{tool}/calculate"),
            &serde_json::json!({}),
            &token,
        )
        .await;
        assert_eq!(status, 404, "{tool} should be gone");
    }
}

#[tokio::test]
#[serial]
async fn test_doc_with_std_curve() {
    if !crate::common::tools_runner::require_runner_or_skip("test_doc_with_std_curve").await {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "doc",
        serde_json::json!({
            "DOC_rep_1": 120.0, "DOC_rep_2": 125.0, "DOC_rep_3": 118.0,
            "std_curve": { "slope": 1.05, "intercept": -2.0 }
        }),
        &token,
    )
    .await;

    assert_value(&json, "DOC_avg_ppb", 125.05);
    assert_value(&json, "DOC_sd_ppb", 3.78582883923719);
}

#[tokio::test]
#[serial]
async fn test_doc_single_replicate_omits_sd() {
    if !crate::common::tools_runner::require_runner_or_skip("test_doc_single_replicate_omits_sd")
        .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "doc",
        serde_json::json!({
            "DOC_rep_1": 120.0
        }),
        &token,
    )
    .await;

    assert_value(&json, "DOC_avg_ppb", 120.0);
    assert_absent(&json, "DOC_sd_ppb");
}

#[tokio::test]
#[serial]
async fn test_tss_afdm() {
    if !crate::common::tools_runner::require_runner_or_skip("test_tss_afdm").await {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "tss_afdm",
        serde_json::json!({
            "wgt_dried_g": 0.1005,
            "wgt_prefilt_g": 0.1,
            "vol_filtered_ml": 500.0
        }),
        &token,
    )
    .await;
    assert_value(&json, "TSS_dry_weight_mgL", 1.0);
    assert_absent(&json, "AFDM_mgL");

    let json = calculate(
        &app,
        "tss_afdm",
        serde_json::json!({
            "wgt_dried_g": 0.1025,
            "wgt_prefilt_g": 0.1,
            "wgt_ashed_g": 0.1005,
            "vol_filtered_ml": 500.0
        }),
        &token,
    )
    .await;
    assert_value(&json, "TSS_dry_weight_mgL", 4.99999999999998);
    assert_value(&json, "AFDM_mgL", 3.99999999999998);
}

#[tokio::test]
#[serial]
async fn test_chlorophyll_acid_and_noacid() {
    if !crate::common::tools_runner::require_runner_or_skip("test_chlorophyll_acid_and_noacid")
        .await
    {
        return;
    }
    // Expected behaviour: one call computes both variants from the same
    // fluorescence pair. Acid is (fluor_1 - fluor_2) * slope + intercept,
    // no-acid is fluor_1 * slope + intercept, each against its own curve.
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "chlorophyll",
        serde_json::json!({
            "lab_chla_fluor_1_rep_A": 150.0,
            "lab_chla_fluor_2_rep_A": 80.0,
            "chla_acid": { "slope": 0.25, "intercept": -1.5 },
            "chla_noacid": { "slope": 0.3, "intercept": -2.0 }
        }),
        &token,
    )
    .await;
    assert_value(&json, "Chla_acid_ugL_avg", 16.0);
    assert_value(&json, "Chla_noacid_ugL_avg", 43.0);
    assert_absent(&json, "Chla_acid_ugL_sd");
    // No rock dimensions or volumes: the per-m2 chain cannot run.
    assert_absent(&json, "Chla_acid_avg_ugm2");
}

#[tokio::test]
#[serial]
async fn test_nutrients_per_replicate_no3() {
    if !crate::common::tools_runner::require_runner_or_skip("test_nutrients_per_replicate_no3")
        .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "nutrients",
        serde_json::json!({
            "NUT_TDN_rep_A": 11.642790355254,
            "NUT_TDN_rep_B": 29.6550577834714,
            "NUT_TDN_rep_C": 43.6351132337004,
            "NUT_NOx_rep_A": 156.276927627623,
            "NUT_NOx_rep_B": 178.205307442695,
            "NUT_NOx_rep_C": 264.759755562991,
            "NUT_NO2_rep_A": 4.18003580882214,
            "NUT_NO2_rep_B": 7.53685696562752,
            "NUT_NO2_rep_C": 9.09055079799145
        }),
        &token,
    )
    .await;

    assert_value(&json, "NUT_TDN_avg", 28.3109871241419);
    assert_value(&json, "NUT_TDN_sd", 16.0384561365065);
    assert_value(&json, "NUT_NOx_avg", 199.747330211103);
    assert_value(&json, "NUT_NOx_sd", 57.3600474889065);
    assert_value(&json, "NUT_NO2_avg", 6.93581452414704);
    assert_value(&json, "NUT_NO2_sd", 2.50982636392635);
    assert_value(&json, "NUT_NO3_rep_A", 152.096891818801);
    assert_value(&json, "NUT_NO3_rep_B", 170.668450477067);
    assert_value(&json, "NUT_NO3_rep_C", 255.669204764999);
    assert_value(&json, "NUT_NO3_avg", 192.811515686956);
    assert_value(&json, "NUT_NO3_sd", 55.2226629647951);
    assert_absent(&json, "NUT_NOX_avg");
}

#[tokio::test]
#[serial]
async fn test_nutrients_old_nutrients_replicates_map_to_ugl_columns() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "test_nutrients_old_nutrients_replicates_map_to_ugl_columns",
    )
    .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "nutrients",
        serde_json::json!({
            "NH4_rep_A": 10.0, "NH4_rep_B": 12.0, "NH4_rep_C": 11.0,
            "SRP_rep_A": 3.0, "SRP_rep_B": 4.0, "SRP_rep_C": 5.0
        }),
        &token,
    )
    .await;

    assert_value(&json, "NH4_avg_ugL", 11.0);
    assert_value(&json, "NH4_sd_ugL", 1.0);
    assert_value(&json, "SRP_avg_ugL", 4.0);
    assert_value(&json, "SRP_sd_ugL", 1.0);
    assert_absent(&json, "NUT_NH4_avg");
    assert_absent(&json, "NUT_SRP_avg");
}

#[tokio::test]
#[serial]
async fn test_nutrients_nut_nh4_replicates_stay_off_the_old_nh4_columns() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "test_nutrients_nut_nh4_replicates_stay_off_the_old_nh4_columns",
    )
    .await
    {
        return;
    }
    let (app, token) = setup().await;

    // NUT_NH4 and the Old Nutrients NH4 are two parameters sharing a spelling, so a value
    // entered under one must reach neither the other's columns nor an SRP column.
    let json = calculate(
        &app,
        "nutrients",
        serde_json::json!({
            "NUT_NH4_rep_A": 10.0, "NUT_NH4_rep_B": 12.0, "NUT_NH4_rep_C": 11.0
        }),
        &token,
    )
    .await;

    assert_value(&json, "NUT_NH4_avg", 11.0);
    assert_value(&json, "NUT_NH4_sd", 1.0);
    assert_absent(&json, "NUT_SRP_avg");
    assert_absent(&json, "NUT_SRP_sd");
    assert_absent(&json, "SRP_avg_ugL");
    assert_absent(&json, "SRP_sd_ugL");
    assert_absent(&json, "NH4_avg_ugL");
    assert_absent(&json, "NUT_NUT_NH4_avg");
}

#[tokio::test]
#[serial]
async fn test_dic_replicates_with_lab_temp() {
    if !crate::common::tools_runner::require_runner_or_skip("test_dic_replicates_with_lab_temp")
        .await
    {
        return;
    }
    let (app, token) = setup().await;

    // Constants (h_co2_29815k, gas_const_r_mol, vial_volume, h3po4_added) come
    // from the seeded constants table, which must carry the portal dump values.
    let json = calculate(
        &app,
        "dic",
        serde_json::json!({
            "lab_dic_acid_sample_wght_rep_A": 11.2234638555674,
            "lab_dic_acid_wght_rep_A": 8.44720867276192,
            "lab_dic_vol_overpressure_rep_A": 0.572314724791795,
            "lab_dic_SA_added_rep_A": 0.215206453308929,
            "lab_dic_co2_dry_rep_A": 7777.55566677079,
            "lab_dic_delta_13co2_rep_A": -17.0359188667499,
            "lab_temp_c": 16.6193622106221,
            "lab_dic_acid_sample_wght_rep_B": 9.90769812220242,
            "lab_dic_acid_wght_rep_B": 8.03407381242141,
            "lab_dic_vol_overpressure_rep_B": 1.5697415038012,
            "lab_dic_SA_added_rep_B": 0.264786635583732,
            "lab_dic_co2_dry_rep_B": 3729.14041790646,
            "lab_dic_delta_13co2_rep_B": -0.847890549339354
        }),
        &token,
    )
    .await;

    assert_value(&json, "DIC_A", 1.51226461877731);
    assert_value(&json, "DIC_B", 1.15725491348659);
    assert_value(&json, "DIC_avg", 1.33475976613195);
    assert_value(&json, "DIC_std", 0.25102976999811);
    assert_value(&json, "d13C_DIC_A", -17.2886319943196);
    assert_value(&json, "d13C_DIC_B", -1.00632845904531);
    assert_value(&json, "d13C_DIC_avg", -9.14748022668244);
    assert_value(&json, "d13C_DIC_std", 11.5133272431301);
}

#[tokio::test]
#[serial]
async fn test_dic_cst_mode_uses_lab_temp_constant() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "test_dic_cst_mode_uses_lab_temp_constant",
    )
    .await
    {
        return;
    }
    // Scenario: the lab-temperature switch set to the constant, no cell entered.
    // Expected behaviour: the lab_temp_avg_degC constant (22.5) drives the
    // calculation; the expected value was generated by R with that value.
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "dic",
        serde_json::json!({
            "lab_temp_mode": "cst",
            "lab_dic_acid_sample_wght_rep_A": 11.5,
            "lab_dic_acid_wght_rep_A": 9.5,
            "lab_dic_vol_overpressure_rep_A": 0.5,
            "lab_dic_SA_added_rep_A": 0.3,
            "lab_dic_co2_dry_rep_A": 2000.0
        }),
        &token,
    )
    .await;

    assert_value(&json, "DIC_avg", 0.516226712592808);
    assert_absent(&json, "d13C_DIC_avg");
}

#[tokio::test]
#[serial]
async fn test_dic_db_mode_with_blank_lab_temp_yields_nothing() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "test_dic_db_mode_with_blank_lab_temp_yields_nothing",
    )
    .await
    {
        return;
    }
    // Scenario: the default mode reads the lab temperature off the row, and the cell is blank.
    // Expected behaviour: no fallback to the constant, so nothing is computed.
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "dic",
        serde_json::json!({
            "lab_dic_acid_sample_wght_rep_A": 11.5,
            "lab_dic_acid_wght_rep_A": 9.5,
            "lab_dic_vol_overpressure_rep_A": 0.5,
            "lab_dic_SA_added_rep_A": 0.3,
            "lab_dic_co2_dry_rep_A": 2000.0
        }),
        &token,
    )
    .await;

    for key in [
        "DIC_A",
        "DIC_B",
        "DIC_avg",
        "DIC_std",
        "d13C_DIC_A",
        "d13C_DIC_B",
        "d13C_DIC_avg",
        "d13C_DIC_std",
    ] {
        assert_absent(&json, key);
    }
}

#[tokio::test]
#[serial]
async fn test_dic_zero_sample_volume_omits_result() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "test_dic_zero_sample_volume_omits_result",
    )
    .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "dic",
        serde_json::json!({
            "lab_dic_acid_sample_wght_rep_A": 9.5,
            "lab_dic_acid_wght_rep_A": 9.5,
            "lab_dic_vol_overpressure_rep_A": 0.5,
            "lab_dic_SA_added_rep_A": 0.3,
            "lab_dic_co2_dry_rep_A": 2000.0,
            "lab_temp_c": 22.0
        }),
        &token,
    )
    .await;

    assert_absent(&json, "DIC_avg");
}

#[tokio::test]
#[serial]
async fn test_pco2_water_temp_at_reference_is_co2_over_solubility() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "test_pco2_water_temp_at_reference_is_co2_over_solubility",
    )
    .await
    {
        return;
    }
    let (app, token) = setup().await;

    // At 25.0 C the van't Hoff exponent is zero, so pCO2 is the headspace CO2 over 0.034.
    let json = calculate(
        &app,
        "pco2",
        serde_json::json!({
            "water_temp_c": 25.0,
            "pressure_hpa": 1038.70408887742,
            "lab_co2_co2ppm_rep_A": 3774.31004084647,
            "lab_co2_h2o_rep_A": 3.04813831089996,
            "lab_co2_ch4_rep_A": 476.103632267332,
            "lab_temp_c": 17.0519462262746,
            "lab_pressure_hpa": 1012.5826292476703
        }),
        &token,
    )
    .await;

    assert_value(&json, "CO2_HS_Um_A", 318.26554063262745);
    assert_value(&json, "pCO2_HS_uatm_A", 318.26554063262745 / 0.034);
}

#[tokio::test]
#[serial]
async fn test_pco2_full_chain_replicates() {
    if !crate::common::tools_runner::require_runner_or_skip("test_pco2_full_chain_replicates").await
    {
        return;
    }
    // Full A/B chain: headspace CO2, pCO2 simple/P1/P2, CH4 dry, dissolved CH4.
    // The syringe and water volumes, like every gas constant here, come from the
    // seeded constants table (portal dump values) and are not caller-settable.
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "pco2",
        serde_json::json!({
            "water_temp_c": 20.6511107342085,
            "pressure_hpa": 1038.70408887742,
            "lab_co2_co2ppm_rep_A": 3774.31004084647,
            "lab_co2_h2o_rep_A": 3.04813831089996,
            "lab_co2_ch4_rep_A": 476.103632267332,
            "lab_temp_c": 17.0519462262746,
            "lab_pressure_hpa": 1012.5826292476703, // 0.999341356277 atm, the fixture's value
            "lab_co2_co2ppm_rep_B": 4934.01614744216,
            "lab_co2_h2o_rep_B": 3.24556990689598,
            "lab_co2_ch4_rep_B": 24.531420403393
        }),
        &token,
    )
    .await;

    assert_value(&json, "CO2_HS_Um_A", 318.26554063262745);
    assert_value(&json, "CO2_HS_Um_B", 416.0567890982303);
    assert_value(&json, "CO2_HS_Um_avg", 367.1611648654289);
    assert_value(&json, "CO2_HS_Um_sd", 69.14885493072633);
    assert_value(&json, "pCO2_HS_uatm_A", 8309.284807103812);
    assert_value(&json, "pCO2_HS_uatm_B", 10862.4212023534);
    assert_value(&json, "pCO2_HS_uatm_avg", 9585.853004728606);
    assert_value(&json, "pCO2_HS_uatm_sd", 1805.3400583751616);
    assert_value(&json, "pCO2_HS_P1_uatm_A", 8518.024283035533);
    assert_value(&json, "pCO2_HS_P1_uatm_B", 11135.298611392309);
    assert_value(&json, "pCO2_HS_P1_uatm_avg", 9826.66144721392);
    assert_value(&json, "pCO2_HS_P1_uatm_sd", 1850.692425806543);
    assert_value(&json, "pCO2_HS_P2_uatm_A", 8105.660621685999);
    assert_value(&json, "pCO2_HS_P2_uatm_B", 10596.230823717751);
    assert_value(&json, "pCO2_HS_P2_uatm_avg", 9350.945722701876);
    assert_value(&json, "pCO2_HS_P2_uatm_sd", 1761.0990788778015);
    assert_value(&json, "lab_co2_ch4_dry_A", 494.014347980239);
    assert_value(&json, "lab_co2_ch4_dry_B", 25.5140767773052);
    assert_value(&json, "CH4_calc_umol_L_A", 21305.1026163270);
    assert_value(&json, "CH4_calc_umol_L_B", 1100.33160830688);
    assert_value(&json, "CH4_umol_L_avg", 11202.7171123169);
    assert_value(&json, "CH4_umol_L_sd", 14286.9305920924);
    assert_absent(&json, "d13C_CO2_avg");
}

#[tokio::test]
#[serial]
async fn test_co2_air_ch4_dry_only() {
    if !crate::common::tools_runner::require_runner_or_skip("test_co2_air_ch4_dry_only").await {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "co2_air",
        serde_json::json!({
            "ch4_wet": 2000.0,
            "h2o_percent": 1.5
        }),
        &token,
    )
    .await;

    assert_value(&json, "lab_co2air_ch4_dry", 2037.009);
    assert_absent(&json, "lab_co2air_co2_dry");
}

#[tokio::test]
#[serial]
async fn test_dom_suva_and_ratios() {
    if !crate::common::tools_runner::require_runner_or_skip("test_dom_suva_and_ratios").await {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "dom",
        serde_json::json!({
            "a254": 0.15,
            "doc_avg_ppb": 2500.0,
            "peak_a": 0.4,
            "peak_c": 0.2,
            "peak_m": 0.25,
            "peak_t": 0.1
        }),
        &token,
    )
    .await;

    assert_value(&json, "SUVA", 0.06);
    assert_value(&json, "A_T", 4.0);
    assert_value(&json, "C_A", 0.5);
    assert_value(&json, "C_M", 0.8);
    assert_value(&json, "C_T", 2.0);
}

#[tokio::test]
#[serial]
async fn test_field_data_baro_and_co2_correction() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "test_field_data_baro_and_co2_correction",
    )
    .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "field_data",
        serde_json::json!({
            "elevation_m": 470.0,
            "temp_c": 15.0
        }),
        &token,
    )
    .await;
    assert_value(&json, "Field_BP_altitude", 958.0);

    // 1013 hPa is inside the 700-1050 band, so the field barometric pressure is
    // used and the correction is the identity at 25 C: 1013 * 1013 * 298 / (1013 * 298).
    let json = calculate(
        &app,
        "field_data",
        serde_json::json!({
            "raw_co2_avg": 1013.0,
            "temp_c": 25.0,
            "field_bp": 1013.0
        }),
        &token,
    )
    .await;
    assert_value(&json, "Vaisala_CO2_avg_corr", 1013.0);
}

#[tokio::test]
#[serial]
async fn test_field_data_reach_depths() {
    if !crate::common::tools_runner::require_runner_or_skip("test_field_data_reach_depths").await {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "field_data",
        serde_json::json!({
            "Reach_depth_rep_1": 42.2791988803074,
            "Reach_depth_rep_2": 9.23158912127838,
            "Reach_depth_rep_3": 35.5728250057437,
            "Reach_depth_rep_4": 47.7065041963942,
            "Reach_depth_rep_5": 59.8868433060125
        }),
        &token,
    )
    .await;

    assert_value(&json, "Reach_depth_avg_cm", 38.9353921019472);
    assert_value(&json, "Reach_depth_sd_cm", 18.846083994371);
}

#[tokio::test]
#[serial]
async fn test_field_data_pressure_selection() {
    if !crate::common::tools_runner::require_runner_or_skip("test_field_data_pressure_selection")
        .await
    {
        return;
    }
    let (app, token) = setup().await;

    // Field BP out of range: falls back to the altitude-derived pressure (958 hPa at 470m/15C).
    let json = calculate(
        &app,
        "field_data",
        serde_json::json!({
            "field_bp": 600.0,
            "elevation_m": 470.0,
            "temp_c": 15.0,
            "raw_co2_avg": 1000.0
        }),
        &token,
    )
    .await;
    let expected = 1000.0 * 958.0 * 298.0 / (1013.0 * (273.0 + 15.0));
    assert_value(&json, "Vaisala_CO2_avg_corr", expected);

    // Field BP in range: wins over the altitude-derived pressure.
    let json = calculate(
        &app,
        "field_data",
        serde_json::json!({
            "field_bp": 950.0,
            "elevation_m": 470.0,
            "temp_c": 15.0,
            "raw_co2_avg": 1000.0
        }),
        &token,
    )
    .await;
    let expected = 1000.0 * 950.0 * 298.0 / (1013.0 * (273.0 + 15.0));
    assert_value(&json, "Vaisala_CO2_avg_corr", expected);
}

#[tokio::test]
#[serial]
async fn test_benthic() {
    if !crate::common::tools_runner::require_runner_or_skip("test_benthic").await {
        return;
    }
    let (app, token) = setup().await;

    // The acid and no-acid chlorophyll variants are separate portal columns, so
    // each replicate carries its own; a single replicate makes the mean equal it.
    let json = calculate(
        &app,
        "benthic",
        serde_json::json!({
            "chla_acid_ugL_rep_A": 77.4592630893458,
            "lab_chla_sizeA_rep_A": 31.3737962753512,
            "lab_chla_sizeB_rep_A": 16.7972950891126,
            "lab_chla_sizeC_rep_A": 23.5402561039664,
            "lab_chla_vol_filtrated_rep_A": 194.788764184341,
            "lab_chla_tot_vol_rep_A": 61.203697801102
        }),
        &token,
    )
    .await;
    assert_value(&json, "chla_acid_ugm2_rep_A", 0.343097523192094);
    assert_value(&json, "Chla_acid_avg_ugm2", 0.343097523192094);
    assert_absent(&json, "benthic_AFDM_avg_gm2");

    let json = calculate(
        &app,
        "benthic",
        serde_json::json!({
            "afdm_g_filter_rep_A": 0.00229464557324536,
            "lab_chla_sizeA_rep_A": 22.226826878963,
            "lab_chla_sizeB_rep_A": 5.00566918659024,
            "lab_chla_sizeC_rep_A": 48.5712060821243,
            "lab_chla_vol_filtrated_rep_A": 147.766697539482,
            "lab_chla_tot_vol_rep_A": 75.3042248194106
        }),
        &token,
    )
    .await;
    assert_value(&json, "benthic_AFDM_avg_gm2", 0.00318747879129481);
}

#[tokio::test]
#[serial]
async fn test_chla_benthic_full_chain() {
    if !crate::common::tools_runner::require_runner_or_skip("test_chla_benthic_full_chain").await {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "chla_benthic",
        serde_json::json!({
            "acid_slope": 0.376772315730341,
            "acid_intercept": -1.83187033934519,
            "noacid_slope": 0.522898836783133,
            "noacid_intercept": 1.0994464524556,
            "replicates": [
                {
                    "fluor_before": 265.574415167794,
                    "fluor_after": 141.826708782464,
                    "vol_total_ml": 308.006711141206,
                    "vol_after_ml": 124.730933833615,
                    "diameters_cm": [12.0199799446855, 31.5325166042894, 20.1821500435472],
                    "afdm_g_filter": 0.00878168280725367
                },
                {
                    "fluor_before": 279.03967986349,
                    "fluor_after": 130.257661403157,
                    "vol_total_ml": 297.664363693912,
                    "vol_after_ml": 89.2195254856027,
                    "diameters_cm": [23.0023935288191, 25.7270285030827, 4.50553066353314],
                    "afdm_g_filter": 0.00130143171176314
                }
            ]
        }),
        &token,
    )
    .await;

    assert_value(&json, "Chla_noacid_ugL_avg", 143.488484846234);
    assert_value(&json, "Chla_noacid_ugL_sd", 4.97871851443831);
    assert_value(&json, "Chla_noacid_avg_ugm2", 4.72170111397028);
    assert_value(&json, "Chla_noacid_sd_ugm2", 0.636780437655608);
    assert_value(&json, "Chla_acid_ugL_avg", 49.5089574283813);
    assert_value(&json, "Chla_acid_ugL_sd", 6.6695978487066);
    assert_value(&json, "Chla_acid_avg_ugm2", 1.63733115106572);
    assert_value(&json, "Chla_acid_sd_ugm2", 0.38237474117737);
    assert_value(&json, "benthic_AFDM_avg_gm2", 0.0313778354472084);
    assert_value(&json, "benthic_AFDM_sd_gm2", 0.0314246916989114);
}

#[tokio::test]
#[serial]
async fn test_chla_benthic_no_acid_omits_acid_keys() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "test_chla_benthic_no_acid_omits_acid_keys",
    )
    .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "chla_benthic",
        serde_json::json!({
            "acid_slope": 0.25,
            "acid_intercept": -1.5,
            "noacid_slope": 0.3,
            "noacid_intercept": -2.0,
            "replicates": [
                {
                    "fluor_before": 150.0,
                    "vol_total_ml": 100.0,
                    "vol_after_ml": 40.0,
                    "diameters_cm": [10.0, 8.0, 6.0]
                }
            ]
        }),
        &token,
    )
    .await;

    assert_value(&json, "Chla_noacid_ugL_avg", 43.0);
    assert_absent(&json, "Chla_acid_ugL_avg");
    assert_absent(&json, "benthic_AFDM_avg_gm2");
    // Single replicate: SD is not computable, so it must be omitted.
    assert_absent(&json, "Chla_noacid_ugL_sd");
}

#[tokio::test]
#[serial]
async fn test_alkalinity_fills_ph_when_missing() {
    if !crate::common::tools_runner::require_runner_or_skip("test_alkalinity_fills_ph_when_missing")
        .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "alkalinity",
        serde_json::json!({
            "Alk_meqL": 2.5,
            "Alk_mgL": 125.1,
            "Alk_w_weight_g": 50.0,
            "Alk_init_pH": 8.1
        }),
        &token,
    )
    .await;

    assert_value(&json, "WTW_pH_1", 8.1);
    assert_value(&json, "Alk_meqL", 2.5);
    assert_value(&json, "Alk_mgL", 125.1);
    assert_value(&json, "Alk_w_weight_g", 50.0);
    assert_absent(&json, "alkalinity_meq_l");
}

#[tokio::test]
#[serial]
async fn test_alkalinity_keeps_existing_ph() {
    if !crate::common::tools_runner::require_runner_or_skip("test_alkalinity_keeps_existing_ph")
        .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "alkalinity",
        serde_json::json!({
            "Alk_init_pH": 8.1,
            "WTW_pH_1": 7.6
        }),
        &token,
    )
    .await;

    assert_value(&json, "WTW_pH_1", 7.6);
}

#[tokio::test]
#[serial]
async fn test_pco2_lab_conditions_come_from_constants_only_in_cst_mode() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "test_pco2_lab_conditions_come_from_constants_only_in_cst_mode",
    )
    .await
    {
        return;
    }
    // Scenario: no lab temperature or pressure entered.
    // Expected behaviour: the seeded constants (lab_temp_avg_degC 22.5,
    // lab_press_avg_atm 0.957237) apply only when the two switches select them.
    // Under the default db mode a blank cell computes nothing.
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "pco2",
        serde_json::json!({
            "water_temp_c": 20.6511107342085,
            "pressure_hpa": 1038.70408887742,
            "lab_co2_co2ppm_rep_A": 3774.31004084647,
            "lab_co2_h2o_rep_A": 3.04813831089996,
            "lab_co2_ch4_rep_A": 476.103632267332,
            "lab_temp_mode": "cst",
            "lab_pressure_mode": "cst",
            "lab_co2_co2ppm_rep_B": 4934.01614744216,
            "lab_co2_h2o_rep_B": 3.24556990689598,
            "lab_co2_ch4_rep_B": 24.531420403393
        }),
        &token,
    )
    .await;

    assert_value(&json, "CO2_HS_Um_A", 280.41423694870065);
    assert_value(&json, "CO2_HS_Um_avg", 323.49470721298854);
    let used = json["inputs_used"].as_array().unwrap();
    assert!(
        !used.iter().any(|k| k == "lab_pressure_hpa"),
        "an input that was not sent is not reported as used: {used:?}"
    );

    let json = calculate(
        &app,
        "pco2",
        serde_json::json!({
            "water_temp_c": 20.6511107342085,
            "pressure_hpa": 1038.70408887742,
            "lab_co2_co2ppm_rep_A": 3774.31004084647,
            "lab_co2_h2o_rep_A": 3.04813831089996,
            "lab_co2_ch4_rep_A": 476.103632267332,
            "lab_co2_co2ppm_rep_B": 4934.01614744216,
            "lab_co2_h2o_rep_B": 3.24556990689598,
            "lab_co2_ch4_rep_B": 24.531420403393
        }),
        &token,
    )
    .await;

    assert_value(&json, "lab_co2_ch4_dry_A", 494.014347980239);
    assert_absent(&json, "CO2_HS_Um_A");
    assert_absent(&json, "CO2_HS_Um_avg");
}

#[tokio::test]
#[serial]
async fn test_pco2_out_of_band_lab_pressure_still_computes() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "test_pco2_out_of_band_lab_pressure_still_computes",
    )
    .await
    {
        return;
    }
    // The 700-1050 hPa band applies to the field barometric pressure only; the lab
    // pressure cell carries no bound, so 1100 hPa is used as entered.
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "pco2",
        serde_json::json!({
            "water_temp_c": 20.6511107342085,
            "pressure_hpa": 1038.70408887742,
            "lab_co2_co2ppm_rep_A": 3774.31004084647,
            "lab_co2_h2o_rep_A": 3.04813831089996,
            "lab_co2_ch4_rep_A": 476.103632267332,
            "lab_temp_c": 17.0519462262746,
            "lab_pressure_hpa": 1100.0,
            "lab_co2_co2ppm_rep_B": 4934.01614744216,
            "lab_co2_h2o_rep_B": 3.24556990689598,
            "lab_co2_ch4_rep_B": 24.531420403393
        }),
        &token,
    )
    .await;

    assert_value(&json, "CO2_HS_Um_A", 345.7417543850244);
    assert_value(&json, "CO2_HS_Um_B", 451.9754287589231);
    assert_value(&json, "CO2_HS_Um_avg", 398.85859157197376);
}

#[tokio::test]
#[serial]
async fn test_alkalinity_echoes_initial_ph() {
    if !crate::common::tools_runner::require_runner_or_skip("test_alkalinity_echoes_initial_ph")
        .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "alkalinity",
        serde_json::json!({ "Alk_meqL": 2.5, "Alk_init_pH": 7.8 }),
        &token,
    )
    .await;
    assert_value(&json, "Alk_init_pH", 7.8);
    assert_value(&json, "WTW_pH_1", 7.8);
}
