//! Exact-value portal-parity tests for the analytical tools. Expected numbers
//! are computed by the verbatim CNET/METALP portal R functions
//! (river-data-core/r_reference/generate_fixtures.R).

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
    assert_eq!(names.len(), 12, "should have 12 tools, got {names:?}");
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
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "doc",
        serde_json::json!({
            "replicates": [120.0, 125.0, 118.0],
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
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "doc",
        serde_json::json!({
            "replicates": [120.0]
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
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "chlorophyll",
        serde_json::json!({
            "method": "acid",
            "fluorescence_before": 150.0,
            "fluorescence_after": 80.0,
            "slope": 0.25,
            "intercept": -1.5
        }),
        &token,
    )
    .await;
    assert_value(&json, "Chla_acid_ugL_avg", 16.0);

    let json = calculate(
        &app,
        "chlorophyll",
        serde_json::json!({
            "method": "no_acid",
            "fluorescence_before": 150.0,
            "slope": 0.3,
            "intercept": -2.0
        }),
        &token,
    )
    .await;
    assert_value(&json, "Chla_noacid_ugL_avg", 43.0);
}

#[tokio::test]
#[serial]
async fn test_nutrients_species_portal_casing_and_per_replicate_no3() {
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "nutrients",
        serde_json::json!({
            "species": {
                "TDN": [11.642790355254, 29.6550577834714, 43.6351132337004],
                "NOx": [156.276927627623, 178.205307442695, 264.759755562991],
                "NO2": [4.18003580882214, 7.53685696562752, 9.09055079799145]
            }
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
    assert_value(&json, "NUT_NO3_avg", 192.811515686956);
    assert_value(&json, "NUT_NO3_sd", 55.2226629647951);
    assert_absent(&json, "NUT_NOX_avg");
}

#[tokio::test]
#[serial]
async fn test_nutrients_legacy_species_map_to_metalp_columns() {
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "nutrients",
        serde_json::json!({
            "species": {
                "NH4": [10.0, 12.0, 11.0],
                "SRP": [3.0, 4.0, 5.0]
            }
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
async fn test_nutrients_nut_prefixed_species_map_to_current_columns() {
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "nutrients",
        serde_json::json!({
            "species": {
                "NUT_NH4": [10.0, 12.0, 11.0],
                "NUT_SRP": [3.0, 4.0, 5.0]
            }
        }),
        &token,
    )
    .await;

    assert_value(&json, "NUT_NH4_avg", 11.0);
    assert_value(&json, "NUT_NH4_sd", 1.0);
    assert_value(&json, "NUT_SRP_avg", 4.0);
    assert_value(&json, "NUT_SRP_sd", 1.0);
    assert_absent(&json, "NH4_avg_ugL");
    assert_absent(&json, "SRP_avg_ugL");
    assert_absent(&json, "NUT_NUT_NH4_avg");
}

#[tokio::test]
#[serial]
async fn test_dic_replicates_with_lab_temp() {
    let (app, token) = setup().await;

    // Constants (h_co2_29815k, gas_const_r_mol, vial_volume, h3po4_added) come
    // from the seeded constants table, which must carry the portal dump values.
    let json = calculate(
        &app,
        "dic",
        serde_json::json!({
            "acid_sample_weight_g": 11.2234638555674,
            "acid_weight_g": 8.44720867276192,
            "vol_overpressure_ml": 0.572314724791795,
            "sa_added_ml": 0.215206453308929,
            "co2_dry_ppm": 7777.55566677079,
            "d13co2_permil": -17.0359188667499,
            "lab_temp_c": 16.6193622106221,
            "replicate_b": {
                "acid_sample_weight_g": 9.90769812220242,
                "acid_weight_g": 8.03407381242141,
                "vol_overpressure_ml": 1.5697415038012,
                "sa_added_ml": 0.264786635583732,
                "co2_dry_ppm": 3729.14041790646,
                "d13co2_permil": -0.847890549339354
            }
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
async fn test_dic_without_lab_temp_falls_back_to_constant() {
    // Scenario: lab_temp_c omitted from the request.
    // Expected behaviour: the portal falls back to the lab_temp_avg_degC
    // constant (22.5); the expected value was generated by R with that fallback.
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "dic",
        serde_json::json!({
            "acid_sample_weight_g": 11.5,
            "acid_weight_g": 9.5,
            "vol_overpressure_ml": 0.5,
            "sa_added_ml": 0.3,
            "co2_dry_ppm": 2000.0
        }),
        &token,
    )
    .await;

    assert_value(&json, "DIC_avg", 0.516226712592808);
    assert_absent(&json, "d13C_DIC_avg");
}

#[tokio::test]
#[serial]
async fn test_dic_zero_sample_volume_omits_result() {
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "dic",
        serde_json::json!({
            "acid_sample_weight_g": 9.5,
            "acid_weight_g": 9.5,
            "vol_overpressure_ml": 0.5,
            "sa_added_ml": 0.3,
            "co2_dry_ppm": 2000.0,
            "lab_temp_c": 22.0
        }),
        &token,
    )
    .await;

    assert_absent(&json, "DIC_avg");
}

#[tokio::test]
#[serial]
async fn test_pco2_simple_mode() {
    let (app, token) = setup().await;

    // At 25.0 C the van't Hoff exponent is zero: pCO2 = 50 / 0.034.
    let json = calculate(
        &app,
        "pco2",
        serde_json::json!({
            "co2_aq_umol": 50.0,
            "water_temp_c": 25.0
        }),
        &token,
    )
    .await;

    assert_value(&json, "pCO2_HS_uatm_avg", 50.0 / 0.034);
}

#[tokio::test]
#[serial]
async fn test_pco2_full_chain_replicates() {
    // Full A/B chain: headspace CO2, pCO2 simple/P1/P2, CH4 dry, dissolved CH4
    // with the portal's 0.957237 lab pressure literal. All gas constants come
    // from the seeded constants table (portal dump values).
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "pco2",
        serde_json::json!({
            "mode": "full_pipeline",
            "water_temp_c": 20.6511107342085,
            "pressure_hpa": 1038.70408887742,
            "co2_ppm": 3774.31004084647,
            "h2o_percent": 3.04813831089996,
            "ch4_ppm": 476.103632267332,
            "lab_temp_c": 17.0519462262746,
            "lab_pressure_hpa": 1012.5826292476703, // 0.999341356277 atm, the fixture's value
            "vol_sa_ml": 0.0489402941428125,
            "vol_water_ml": 0.0271797092910856,
            "replicate_b": {
                "co2_ppm": 4934.01614744216,
                "h2o_percent": 3.24556990689598,
                "ch4_ppm": 24.531420403393
            }
        }),
        &token,
    )
    .await;

    assert_value(&json, "CO2_HS_Um_A", 445.077179799069);
    assert_value(&json, "CO2_HS_Um_B", 581.832962374793);
    assert_value(&json, "CO2_HS_Um_avg", 513.455071086931);
    assert_value(&json, "CO2_HS_Um_sd", 96.7009412257675);
    assert_value(&json, "pCO2_HS_uatm_A", 11620.0862988240);
    assert_value(&json, "pCO2_HS_uatm_B", 15190.5097388902);
    assert_value(&json, "pCO2_HS_uatm_avg", 13405.2980188571);
    assert_value(&json, "pCO2_HS_uatm_sd", 2524.67062617817);
    assert_value(&json, "pCO2_HS_P1_uatm_A", 11911.9971889435);
    assert_value(&json, "pCO2_HS_P1_uatm_B", 15572.1140665359);
    assert_value(&json, "pCO2_HS_P1_uatm_avg", 13742.0556277397);
    assert_value(&json, "pCO2_HS_P1_uatm_sd", 2588.09346408091);
    assert_value(&json, "pCO2_HS_P2_uatm_A", 11335.3288663841);
    assert_value(&json, "pCO2_HS_P2_uatm_B", 14818.2568623227);
    assert_value(&json, "pCO2_HS_P2_uatm_avg", 13076.7928643534);
    assert_value(&json, "pCO2_HS_P2_uatm_sd", 2462.80200431262);
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

    let json = calculate(
        &app,
        "field_data",
        serde_json::json!({
            "raw_co2": 1013.0,
            "temp_c": 25.0,
            "pressure_hpa": 1013.0
        }),
        &token,
    )
    .await;
    assert_value(&json, "Vaisala_CO2_avg_corr", 1013.0);
}

#[tokio::test]
#[serial]
async fn test_field_data_reach_depths() {
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "field_data",
        serde_json::json!({
            "reach_depths": [42.2791988803074, 9.23158912127838, 35.5728250057437,
                             47.7065041963942, 59.8868433060125]
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
    let (app, token) = setup().await;

    // Field BP out of range: falls back to the altitude-derived pressure (958 hPa at 470m/15C).
    let json = calculate(
        &app,
        "field_data",
        serde_json::json!({
            "field_bp": 600.0,
            "elevation_m": 470.0,
            "temp_c": 15.0,
            "raw_co2": 1000.0
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
            "raw_co2": 1000.0
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
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "benthic",
        serde_json::json!({
            "chla_ug_l": 77.4592630893458,
            "diameters_cm": [31.3737962753512, 16.7972950891126, 23.5402561039664],
            "volume_filtered_ml": 194.788764184341,
            "total_volume_ml": 61.203697801102
        }),
        &token,
    )
    .await;
    assert_value(&json, "Chla_avg_ugm2", 0.343097523192094);
    assert_absent(&json, "benthic_AFDM_avg_gm2");

    let json = calculate(
        &app,
        "benthic",
        serde_json::json!({
            "afdm_g_filter": 0.00229464557324536,
            "diameters_cm": [22.226826878963, 5.00566918659024, 48.5712060821243],
            "volume_filtered_ml": 147.766697539482,
            "total_volume_ml": 75.3042248194106
        }),
        &token,
    )
    .await;
    assert_value(&json, "benthic_AFDM_avg_gm2", 0.00318747879129481);
}

#[tokio::test]
#[serial]
async fn test_chla_benthic_full_chain() {
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
async fn test_pco2_lab_conditions_fall_back_to_constants() {
    // Blank lab entries resolve from the seeded constants (lab_temp_avg_degC 22.5,
    // lab_press_avg_atm 0.957237, vol_sa/vol_water 0.03), the portal's calcCO2 defaults.
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "pco2",
        serde_json::json!({
            "mode": "full_pipeline",
            "water_temp_c": 20.0,
            "pressure_hpa": 1013.0,
            "co2_ppm": 3000.0,
            "h2o_percent": 3.0,
            "ch4_ppm": 400.0,
        }),
        &token,
    )
    .await;

    assert!(
        json["results"]["CO2_HS_Um_avg"].as_f64().is_some(),
        "the run resolves its lab conditions from constants: {json}"
    );
    let used = json["inputs_used"].as_array().unwrap();
    assert!(
        !used.iter().any(|k| k == "lab_pressure_hpa"),
        "an input that was not sent is not reported as used: {used:?}"
    );
}

#[tokio::test]
#[serial]
async fn test_pco2_rejects_an_atm_style_lab_pressure() {
    // 0.96 is a plausible atm entry and an impossible hPa one; without the band the result is
    // silently ~1000x wrong.
    let (app, token) = setup().await;

    let (status, json) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/pco2/calculate",
        &serde_json::json!({
            "mode": "full_pipeline",
            "water_temp_c": 20.0,
            "pressure_hpa": 1013.0,
            "co2_ppm": 3000.0,
            "h2o_percent": 3.0,
            "ch4_ppm": 400.0,
            "lab_pressure_hpa": 0.96,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "out-of-band lab pressure is refused: {json:?}");
}

#[tokio::test]
#[serial]
async fn test_pco2_simple_mode_reports_a_discarded_replicate() {
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "pco2",
        serde_json::json!({
            "mode": "simple",
            "co2_aq_umol": 100.0,
            "water_temp_c": 15.0,
            "replicate_b": { "co2_ppm": 1.0, "h2o_percent": 1.0, "ch4_ppm": 1.0 },
        }),
        &token,
    )
    .await;

    let ignored = json["inputs_ignored"].as_array().unwrap();
    assert!(
        ignored.iter().any(|k| k == "replicate_b"),
        "simple mode discards replicate_b and says so: {json}"
    );
    let used = json["inputs_used"].as_array().unwrap();
    assert!(
        !used.iter().any(|k| k == "replicate_b"),
        "a discarded input is not reported as used: {used:?}"
    );
}

#[tokio::test]
#[serial]
async fn test_alkalinity_echoes_initial_ph() {
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

#[tokio::test]
#[serial]
async fn test_co2_air_headspace_from_lab_entry() {
    // calcCO2 with explicit lab conditions: 3000 ppm at 0.999 atm equivalent, 20 degC lab,
    // 0.03/0.03 volumes. Expected value computed from the R formula:
    // exponent = exp(2400 * (1/293.15 - 1/298.15)) = exp(0.13727...) -> 1.147137...
    // CO2 = 3000 * P_atm * (0.03 + 0.034 * exponent * 0.03 * 0.0820574 * 293.15)
    //       / (0.0820574 * 0.03 * 293.15)
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "co2_air",
        serde_json::json!({
            "ch4_wet": 400.0,
            "h2o_percent": 3.0,
            "co2_ppm": 3000.0,
            "lab_temp_c": 20.0,
            "lab_pressure_hpa": 1013.25,
            "vol_sa_ml": 0.03,
            "vol_water_ml": 0.03,
        }),
        &token,
    )
    .await;

    let exponent = (2400.0f64 * (1.0 / 293.15 - 1.0 / 298.15)).exp();
    let expected = 3000.0 * 1.0 * (0.03 + 0.034 * exponent * 0.03 * 0.0820574 * 293.15)
        / (0.0820574 * 0.03 * 293.15);
    assert_value(&json, "CO2_HS_Um", expected);
}
