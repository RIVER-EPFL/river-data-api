//! `readings.ingested_at` is arrival provenance, and a full-content re-assert pass must not move
//! it. CNET and METALP re-send their whole content every cycle, so an overwrite carrying the value
//! a row already holds keeps that row's original arrival stamp: the guard is the CASE in
//! `readings_upsert` that re-stamps only when `raw_value`/`calibrated_value` changes. Inverting or
//! dropping that condition re-stamps every re-asserted row, destroying arrival provenance for the
//! whole portal dataset with nothing else failing.
//!
//! The change side is the arrival of the current value: an overwrite that does change a value
//! records a value-correction decision, and its projection trigger re-stamps `ingested_at` as it
//! writes the new `raw_value`, so the correction reports when it arrived rather than when the value
//! it replaced first landed.
//!
//! Run: cargo test --test readings ingested_at_restamp -- --test-threads=1

use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const AT: &str = "2025-06-15T10:00:00Z";

async fn setup() -> (axum::Router, sea_orm::DatabaseConnection, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    (crate::common::build_test_app(db.clone()), db, token)
}

async fn ingest(app: &axum::Router, token: &str, value: f64, conflict: &str) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "time": AT,
                "raw_value": value,
            }],
            "conflict": conflict,
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "batch ({status}): {body}");
    body
}

async fn ingested_at_epoch(db: &sea_orm::DatabaseConnection) -> f64 {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT count(*) AS n, \
                        max(extract(epoch FROM ingested_at)::double precision) AS ts \
                 FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
                 AND time = '{AT}'"
            ),
        ))
        .await
        .expect("query ingested_at")
        .expect("reading row present");
    assert_eq!(
        row.try_get::<i64>("", "n").expect("count"),
        1,
        "exactly one reading at the slot"
    );
    row.try_get::<f64>("", "ts").expect("ingested_at not null")
}

#[tokio::test]
#[serial]
async fn identical_reassert_keeps_the_original_arrival_stamp() {
    let (app, db, token) = setup().await;

    let first = ingest(&app, &token, 1.0, "skip").await;
    assert_eq!(first["inserted"], 1, "{first}");
    let original = ingested_at_epoch(&db).await;

    // A measurable gap so that a wrongly re-stamped row would land at an unmistakably later time
    // rather than within the same clock tick as the original insert.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let reassert = ingest(&app, &token, 1.0, "overwrite").await;
    assert_eq!(reassert["overwritten"], 0, "identical content changes nothing: {reassert}");

    let after = ingested_at_epoch(&db).await;
    assert_eq!(
        after, original,
        "a full-content re-assert of the value a row already holds must not move its arrival stamp"
    );
}

#[tokio::test]
#[serial]
async fn value_correction_moves_the_arrival_stamp() {
    let (app, db, token) = setup().await;

    let first = ingest(&app, &token, 1.0, "skip").await;
    assert_eq!(first["inserted"], 1, "{first}");
    let original = ingested_at_epoch(&db).await;

    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let corrected = ingest(&app, &token, 2.0, "overwrite").await;
    assert_eq!(corrected["overwritten"], 1, "the correction changes the value: {corrected}");

    let after = ingested_at_epoch(&db).await;
    assert!(
        after > original,
        "a correction that changes the stored value is a fresh arrival: {after} <= {original}"
    );
}
