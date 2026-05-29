mod common;

use serial_test::serial;

async fn setup() -> (axum::Router, String) {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db);
    (app, token)
}

fn assert_finite(json: &serde_json::Value, key: &str) {
    let v = &json["results"][key];
    assert!(v.is_f64() || v.is_i64(), "{key} should be a number, got: {v}");
    if let Some(f) = v.as_f64() {
        assert!(f.is_finite(), "{key} should be finite, got: {f}");
    }
}

fn assert_result_key_exists(json: &serde_json::Value, key: &str) {
    assert!(
        !json["results"][key].is_null(),
        "{key} should exist in results, got: {:?}",
        json["results"]
    );
}

// ============================================================================
// Tool list
// ============================================================================

#[tokio::test]
#[serial]
async fn test_list_tools() {
    let (app, token) = setup().await;

    let (status, json) = common::get_json_with_token(&app, "/api/tools", &token).await;
    assert_eq!(status, 200);

    let tools = json.as_array().expect("tools should be an array");
    assert_eq!(tools.len(), 14, "should have 14 tools");

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"doc"));
    assert!(names.contains(&"ions"));
    assert!(names.contains(&"alkalinity"));
    assert!(names.contains(&"pco2"));
    assert!(names.contains(&"dic"));
    assert!(names.contains(&"field_data"));
    assert!(names.contains(&"isotopes"));
    assert!(names.contains(&"chla_benthic"));
}

// ============================================================================
// DOC — replicate absorbance values from CNET lab analysis
// ============================================================================

#[tokio::test]
#[serial]
async fn test_doc_with_replicates() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &serde_json::json!({
            "replicates": [185.2, 198.7, 191.4]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "doc");
    assert_finite(&json, "DOC_avg_ppb");
    assert_finite(&json, "DOC_sd_ppb");

    let avg = json["results"]["DOC_avg_ppb"].as_f64().unwrap();
    assert!((avg - 191.77).abs() < 1.0, "DOC avg should be ~191.8 ppb, got {avg}");
}

#[tokio::test]
#[serial]
async fn test_doc_with_std_curve() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &serde_json::json!({
            "replicates": [0.45, 0.52, 0.48],
            "std_curve": { "slope": 412.5, "intercept": -3.2 }
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_finite(&json, "DOC_avg_ppb");
    assert_finite(&json, "DOC_sd_ppb");
}

// ============================================================================
// Alkalinity — Gran titration with realistic Swiss alpine water
// ============================================================================

#[tokio::test]
#[serial]
async fn test_alkalinity() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/alkalinity/calculate",
        &serde_json::json!({
            "sample_weight_g": 50.0,
            "acid_normality": 0.02,
            "titrant_volume_ml": 12.5,
            "initial_ph": 8.1
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "alkalinity");
    assert_finite(&json, "alkalinity_meq_l");
    assert_finite(&json, "alkalinity_mg_l_caco3");
    assert_eq!(json["results"]["WTW_pH_1"], 8.1);
}

// ============================================================================
// Ions — charge balance with real CNET ion concentrations
// ============================================================================

#[tokio::test]
#[serial]
async fn test_ion_charge_balance() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/ions/calculate",
        &serde_json::json!({
            "cations": [
                { "name": "Ca", "concentration_mg_l": 25.66 },
                { "name": "Mg", "concentration_mg_l": 6.56 },
                { "name": "Na", "concentration_mg_l": 0.86 },
                { "name": "K", "concentration_mg_l": 0.85 }
            ],
            "anions": [
                { "name": "SO4", "concentration_mg_l": 42.59 },
                { "name": "Cl", "concentration_mg_l": 0.17 },
                { "name": "HCO3", "concentration_mg_l": 50.0 }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "ions");
    assert_finite(&json, "sum_cations_meq");
    assert_finite(&json, "sum_anions_meq");
    assert_finite(&json, "balance_percent");

    let balance = json["results"]["balance_percent"].as_f64().unwrap();
    assert!(balance.abs() < 25.0, "balance should be reasonable, got {balance}%");
}

// ============================================================================
// Isotopes — typical Swiss alpine water values
// ============================================================================

#[tokio::test]
#[serial]
async fn test_isotopes() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/isotopes/calculate",
        &serde_json::json!({
            "d_d": -102.5,
            "d18o": -13.98,
            "d17o": -7.42
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "isotopes");
    assert_finite(&json, "d_excess");
    assert_result_key_exists(&json, "o17_excess_permeg");

    let d_excess = json["results"]["d_excess"].as_f64().unwrap();
    assert!(
        (d_excess - 9.34).abs() < 1.0,
        "d-excess should be ~9.3 (d_D - 8*d18O), got {d_excess}"
    );
}

// ============================================================================
// Field Data — Swiss alpine site with barometric pressure + CO2 correction
// ============================================================================

#[tokio::test]
#[serial]
async fn test_field_data() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/field_data/calculate",
        &serde_json::json!({
            "elevation_m": 1936.0,
            "temp_c": 6.7,
            "raw_co2_avg": 512.0,
            "raw_co2_min": 450.0,
            "raw_co2_max": 580.0,
            "pressure_hpa": 800.0,
            "reach_depths": [35.0, 28.5, 42.0, 31.0, 38.5]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "field_data");
    assert_finite(&json, "Field_BP_altitude");
    assert_finite(&json, "Vaisala_CO2_avg_corr");
    assert_finite(&json, "Vaisala_CO2_min_corr");
    assert_finite(&json, "Vaisala_CO2_max_corr");
    assert_finite(&json, "Reach_depth_avg_cm");
    assert_finite(&json, "Reach_depth_sd_cm");

    let bp = json["results"]["Field_BP_altitude"].as_f64().unwrap();
    assert!(bp > 700.0 && bp < 850.0, "barometric pressure at 1936m should be ~790 hPa, got {bp}");
}

// ============================================================================
// pCO2 — simple mode with dissolved CO2 from Vaisala sensor
// ============================================================================

#[tokio::test]
#[serial]
async fn test_pco2_simple() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/pco2/calculate",
        &serde_json::json!({
            "co2_aq_umol": 15.0,
            "water_temp_c": 6.7,
            "variant": "simple"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "pco2");
    assert_finite(&json, "pCO2_HS_uatm_avg");
}

// ============================================================================
// DIC — acid digestion with realistic lab values
// ============================================================================

#[tokio::test]
#[serial]
async fn test_dic() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/dic/calculate",
        &serde_json::json!({
            "acid_sample_weight_g": 5.02,
            "acid_weight_g": 0.25,
            "vol_overpressure_ml": 40.0,
            "sa_added_ml": 20.0,
            "co2_dry_ppm": 8500.0,
            "d13co2_permil": -12.5,
            "lab_temp_c": 22.0
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "dic");
    assert_finite(&json, "DIC_avg");
    assert_result_key_exists(&json, "d13C_DIC_avg");
}

// ============================================================================
// Nutrients — multi-species mode with CNET-realistic concentrations
// ============================================================================

#[tokio::test]
#[serial]
async fn test_nutrients_multi_species() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/nutrients/calculate",
        &serde_json::json!({
            "species": {
                "PO4": [0.008, 0.009, 0.007],
                "NH4": [0.015, 0.018, 0.016],
                "NO3": [1.85, 1.92, 1.88]
            }
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "nutrients");
    assert_finite(&json, "NUT_PO4_avg");
    assert_finite(&json, "NUT_PO4_sd");
    assert_finite(&json, "NUT_NH4_avg");
    assert_finite(&json, "NUT_NO3_avg");
}

// ============================================================================
// TSS/AFDM — filter weight analysis
// ============================================================================

#[tokio::test]
#[serial]
async fn test_tss_afdm() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/tss_afdm/calculate",
        &serde_json::json!({
            "wgt_dried_g": 0.1025,
            "wgt_prefilt_g": 0.1000,
            "wgt_ashed_g": 0.1012,
            "vol_filtered_ml": 500.0
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "tss_afdm");
    assert_finite(&json, "TSS_dry_weight_mgL");
    assert_result_key_exists(&json, "AFDM_mgL");

    let tss = json["results"]["TSS_dry_weight_mgL"].as_f64().unwrap();
    assert!(tss > 0.0, "TSS should be positive, got {tss}");
}

// ============================================================================
// DOM — SUVA and fluorescence peaks from CNET UV-Vis data
// ============================================================================

#[tokio::test]
#[serial]
async fn test_dom_indices() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/dom/calculate",
        &serde_json::json!({
            "a254": 0.085,
            "doc_avg_ppb": 192.0,
            "peak_a": 0.054,
            "peak_c": 0.018,
            "peak_m": 0.037,
            "peak_t": 0.053
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "dom");
    assert_result_key_exists(&json, "SUVA");
    assert_result_key_exists(&json, "A_T");
    assert_result_key_exists(&json, "C_A");
    assert_result_key_exists(&json, "C_M");
    assert_result_key_exists(&json, "C_T");
}

// ============================================================================
// CO2 Air — wet-to-dry conversion
// ============================================================================

#[tokio::test]
#[serial]
async fn test_co2_air() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/co2_air/calculate",
        &serde_json::json!({
            "co2_wet": 415.0,
            "ch4_wet": 1.95,
            "h2o_percent": 1.2
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "co2_air");
    assert_result_key_exists(&json, "lab_co2air_co2_dry");
    assert_result_key_exists(&json, "lab_co2air_ch4_dry");
}

// ============================================================================
// Chlorophyll — acid method
// ============================================================================

#[tokio::test]
#[serial]
async fn test_chlorophyll_acid() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/chlorophyll/calculate",
        &serde_json::json!({
            "method": "acid",
            "fluorescence_before": 45.2,
            "fluorescence_after": 22.1,
            "slope": 0.5,
            "intercept": -0.1
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "chlorophyll");
    assert_finite(&json, "Chla_acid_ugL_avg");
}

// ============================================================================
// Benthic — rock surface area with alpine stream dimensions
// ============================================================================

#[tokio::test]
#[serial]
async fn test_benthic() {
    let (app, token) = setup().await;

    let (status, json) = common::post_json_parse_with_token(
        &app,
        "/api/tools/benthic/calculate",
        &serde_json::json!({
            "diameters_cm": [8.5, 12.0, 6.3, 9.8, 7.2],
            "afdm_g_filter": 0.0035,
            "chla_ug_l": 2.8,
            "volume_filtered_ml": 100.0,
            "total_volume_ml": 250.0
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["tool"], "benthic");
    assert_finite(&json, "rock_surface_area_m2");
    assert_result_key_exists(&json, "benthic_AFDM_avg_gm2");
    assert_result_key_exists(&json, "chla_per_m2");
}

// ============================================================================
// Unknown tool returns 404
// ============================================================================

#[tokio::test]
#[serial]
async fn test_unknown_tool() {
    let (app, token) = setup().await;

    let (status, _) = common::post_json_with_token(
        &app,
        "/api/tools/nonexistent/calculate",
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 404);
}

// ============================================================================
// Permission check — read_data required for tools
// ============================================================================

#[tokio::test]
#[serial]
async fn test_tools_require_valid_token() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let app = common::build_test_app(db);

    let (status, _) = common::post_json_with_token(
        &app,
        "/api/tools/doc/calculate",
        &serde_json::json!({ "replicates": [100.0] }),
        "invalid-token",
    )
    .await;
    assert_eq!(status, 401, "tools should require valid token");
}
