//! Full ingest → query round-trip across separate keys, proving the capability bits are truly
//! independent: a `write_data`-only key can push data but cannot read it back; a separate
//! `read_data` key reads the same point and the value matches.

use serial_test::serial;

use crate::common::fixtures::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

const WRITE_TIME: &str = "2025-03-03T03:03:03Z";
const WRITE_VALUE: f64 = 88.5;
const READ_WINDOW: &str = "start=2025-03-03T00:00:00Z&end=2025-03-03T06:00:00Z";

/// Whether any value in a `/sites/{id}/readings` JSON response is approximately `target`.
fn readings_contain_value(body: &serde_json::Value, target: f64) -> bool {
    fn walk(v: &serde_json::Value, target: f64) -> bool {
        match v {
            serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| (f - target).abs() < 1e-6),
            serde_json::Value::Array(a) => a.iter().any(|x| walk(x, target)),
            serde_json::Value::Object(o) => o.values().any(|x| walk(x, target)),
            _ => false,
        }
    }
    walk(body, target)
}

#[tokio::test]
#[serial]
async fn write_only_key_cannot_read_back_separate_read_key_can() {
    let (db, app) = setup().await;

    let writer =
        crate::common::seed_api_token(&db, crate::common::perms(false, false, false, true), None)
            .await;
    let reader =
        crate::common::seed_api_token(&db, crate::common::perms(false, true, false, false), None)
            .await;

    // Writer pushes a distinctive point.
    let batch = serde_json::json!({
        "readings": [{ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": WRITE_TIME, "raw_value": WRITE_VALUE }]
    });
    let (s, body) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &batch, &writer).await;
    assert_eq!(s, 200, "write_data key must ingest: {body}");

    let readings_url = format!(
        "/api/sites/{SITE1_ID}/readings?{READ_WINDOW}&parameter_ids={GLOBAL_PARAM_TEMP_ID}"
    );

    // The writer cannot read its own data back; it has no read_data.
    let (s, _) = crate::common::get_with_token(&app, &readings_url, &writer).await;
    assert_eq!(
        s, 403,
        "write_data-only key must be denied reading data, got {s}"
    );

    // A separate read_data key reads it back, and the value matches.
    let (s, body) = crate::common::get_json_with_token(&app, &readings_url, &reader).await;
    assert_eq!(s, 200, "read_data key must read the data back: {body}");
    assert!(
        readings_contain_value(&body, WRITE_VALUE),
        "the value just written ({WRITE_VALUE}) must round-trip back: {body}"
    );

    // And the read key cannot write.
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &batch, &reader).await;
    assert_eq!(s, 403, "read_data-only key must be denied ingest, got {s}");
}
