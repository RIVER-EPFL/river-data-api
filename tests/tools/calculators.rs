//! The tools list and `/tools/{name}/calculate`: the reference set is listed, retired tools answer
//! 404, and `doc` runs. `doc`'s script returns nothing of its own, so its numbers are the engine's
//! mean and sample sd over the replicates after the standard curve, not portal R arithmetic.

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
    // same database and cleanup does not remove them, so assert the reference set is served.
    // `doc` is the one R tool the fixtures carry; the portal's other calculators are formula
    // calculations authored by hand and reach no database from here.
    {
        let expected = "doc";
        assert!(names.contains(&expected), "missing tool {expected}");
    }
    assert!(!names.contains(&"ions"));
    assert!(!names.contains(&"isotopes"));
    // Retired: the portal has one Chl a tool, and chlorophyll is it.
    assert!(!names.contains(&"chla_benthic"));
}

#[tokio::test]
#[serial]
async fn test_removed_tools_return_404() {
    let (app, token) = setup().await;

    // Tools and helper calculations dropped for lacking a portal counterpart:
    // the ion charge balance, the isotope excesses, Gran titration alkalinity,
    // the dry-CO2 correction, spectral slope, and percent organic.
    for tool in [
        "ions",
        "isotopes",
        "gran_titration",
        "co2_dry",
        "spectral_slope",
        "percent_organic",
        "chla_benthic",
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
    if !crate::common::profile::Service::ToolsRunner
        .require("test_doc_with_std_curve")
        .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "doc",
        serde_json::json!({
            "DOC": [120.0, 125.0, 118.0],
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
    if !crate::common::profile::Service::ToolsRunner
        .require("test_doc_single_replicate_omits_sd")
        .await
    {
        return;
    }
    let (app, token) = setup().await;

    let json = calculate(
        &app,
        "doc",
        serde_json::json!({
            "DOC": [120.0]
        }),
        &token,
    )
    .await;

    assert_value(&json, "DOC_avg_ppb", 120.0);
    assert_absent(&json, "DOC_sd_ppb");
}
