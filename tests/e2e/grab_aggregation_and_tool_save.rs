//! End-to-end: grab-sample entry with replicate aggregation, then an analytical tool calculation
//! whose result is saved back to the station as a grab sample (the "Save to Station" flow, WS5).
//!
//! Run: cargo test --test e2e -- --test-threads=1


use crate::common::e2e;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn grab_replicates_aggregate_then_tool_result_saved_to_station() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site1 = crate::common::SITE1_ID;
    let param = crate::common::GLOBAL_PARAM_TEMP_ID; // seeded as a site_parameter at site 1
    let vals = [185.2_f64, 198.7, 191.4];

    // 1. Three replicates at one timestamp → one sample with trigger-maintained aggregates.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": site1, "created_by": "e2e",
            "readings": [
                { "parameter_id": param, "value": vals[0], "time": "2025-06-15T10:00:00Z" },
                { "parameter_id": param, "value": vals[1], "time": "2025-06-15T10:00:00Z" },
                { "parameter_id": param, "value": vals[2], "time": "2025-06-15T10:00:00Z" },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab_samples ({status}): {body}");
    let gj: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(gj["inserted"], 3, "three replicate readings inserted");
    assert_eq!(gj["samples_created"], 1, "three replicates collapse into one sample");

    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT n, mean, min_value, max_value FROM samples WHERE site_id='{site1}' AND parameter_id='{param}'"),
        ))
        .await
        .unwrap()
        .expect("sample row exists");
    let n: i32 = row.try_get("", "n").unwrap();
    let mean: f64 = row.try_get::<Option<f64>>("", "mean").unwrap().expect("trigger populates mean");
    let minv: f64 = row.try_get::<Option<f64>>("", "min_value").unwrap().unwrap();
    let maxv: f64 = row.try_get::<Option<f64>>("", "max_value").unwrap().unwrap();
    assert_eq!(n, 3, "n = 3 replicates");
    assert!((mean - vals.iter().sum::<f64>() / 3.0).abs() < 1e-6, "mean of replicates");
    assert!((minv - 185.2).abs() < 1e-6 && (maxv - 198.7).abs() < 1e-6, "min/max of replicates");

    // 2. Analytical tool calculation (DOC from replicates) → numeric result.
    let (status, doc) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &serde_json::json!({ "replicates": vals }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "doc calculate ({status}): {doc}");
    assert_eq!(doc["tool"], "doc");
    let doc_avg = doc["results"]["DOC_avg_ppb"].as_f64().expect("DOC_avg_ppb result");
    assert!(doc_avg.is_finite() && doc_avg > 0.0, "DOC result should be a positive number");

    // 3. Save to Station: persist the tool result as a grab sample at a site_parameter (mirrors the
    //    SaveToStationDialog flow). Use a distinct timestamp so it's isolatable.
    let (status, save) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": site1, "created_by": "e2e-tool",
            "readings": [ { "parameter_id": param, "value": doc_avg, "time": "2025-06-15T11:00:00Z" } ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "save to station ({status}): {save}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&save).unwrap()["inserted"], 1);

    // 4. The saved tool result is queryable as a reading (window excludes the 10:00 replicates).
    let uri = format!("/api/sites/{site1}/readings?start=2025-06-15T10:30:00Z&end=2025-06-15T11:30:00Z");
    let (status, readings) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "readings ({status}): {readings}");
    let got = e2e::values_for(&readings, param);
    assert!(
        got.iter().any(|v| (v - doc_avg).abs() < 1e-6),
        "saved tool value {doc_avg} should appear in readings, got {got:?}"
    );
}
