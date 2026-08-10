//! H13 — flagging a reading must refresh the continuous aggregates so the flagged value stops being
//! served from the rollups. The refresh window is widened to a full bucket; before the fix a
//! single-reading `[t, t+1s)` window was narrower than every aggregate's bucket and TimescaleDB
//! rejected it, leaving the rollup either stale or (as here) never materialised.
//!
//! Run: cargo test --test readings aggregate_refresh_on_flag -- --test-threads=1

use crate::common::sensor_lifecycle::create_paired_stream;
use crate::common::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn hourly(db: &DatabaseConnection, bucket: &str) -> Option<(f64, i64)> {
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT avg_value, count FROM readings_hourly \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
                   AND bucket = '{bucket}'"
            ),
        ))
        .await
        .expect("query readings_hourly")?;
    Some((row.try_get("", "avg_value").unwrap(), row.try_get("", "count").unwrap()))
}

#[tokio::test]
#[serial]
async fn flagging_a_reading_refreshes_the_hourly_aggregate() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;

    let stream = create_paired_stream(&db, "flag-temp", PARAM_S1_TEMP_ID).await;
    // Five readings in one hour: 10,20,30,40,50. Flag the 50 → the hourly mean of the rest is 25.
    for (i, v) in [10.0, 20.0, 30.0, 40.0, 50.0].iter().enumerate() {
        exec(
            &db,
            &format!(
                "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index) \
                 VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', \
                         '2025-06-15T10:0{i}:00Z', {v}, {v}, 0)"
            ),
        )
        .await;
    }

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;
    let (status, body) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &serde_json::json!({
            "readings": [
                { "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": "2025-06-15T10:04:00Z" }
            ],
            "reason": "spike"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "flag: {body}");

    // The flag endpoint's aggregate refresh must have materialised the bucket excluding the flagged
    // reading. Before the fix the refresh CALL failed (window too small) and this row never appeared.
    let (avg, count) = hourly(&db, "2025-06-15T10:00:00Z")
        .await
        .expect("hourly bucket must exist after the flag-triggered refresh");
    assert_eq!(count, 4, "the flagged reading is excluded from the rollup");
    assert!((avg - 25.0).abs() < 1e-9, "hourly mean excludes the flagged 50: got {avg}");

    cleanup_test_db(&db).await;
}
