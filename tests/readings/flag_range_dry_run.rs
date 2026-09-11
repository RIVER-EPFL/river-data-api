//! `dry_run: true` on `/readings/flag_range` and `/readings/unflag_range` returns the count the
//! write would report and changes nothing: no flag, no decision row, and nothing the rollups
//! serve.
//!
//! Run: cargo test --test readings flag_range_dry_run -- --test-threads=1

use crate::common::e2e::count;
use crate::common::sensor_lifecycle::create_paired_stream;
use crate::common::*;
use serial_test::serial;

fn slot() -> String {
    format!("site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}'")
}

async fn seed_five(db: &sea_orm::DatabaseConnection) {
    let stream = create_paired_stream(db, "dry-run-temp", PARAM_S1_TEMP_ID).await;
    for (i, v) in [10.0, 20.0, 30.0, 40.0, 50.0].iter().enumerate() {
        exec(
            db,
            &format!(
                "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index) \
                 VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', \
                         '2025-06-15T10:0{i}:00Z', {v}, {v}, 0)"
            ),
        )
        .await;
    }
}

fn range(dry_run: bool, reason: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "site_id": SITE1_ID,
        "parameter_id": GLOBAL_PARAM_TEMP_ID,
        "start_time": "2025-06-15T10:01:00Z",
        "end_time": "2025-06-15T10:03:00Z",
        "dry_run": dry_run,
    });
    if let Some(reason) = reason {
        body["reason"] = serde_json::Value::String(reason.to_string());
    }
    body
}

#[tokio::test]
#[serial]
async fn flag_range_dry_run_counts_without_writing() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    seed_five(&db).await;
    let slot = slot();
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    // A dry run needs no reason: the dialog asks before the reason is typed.
    let (status, body) =
        patch_json_with_token(&app, "/api/readings/flag_range", &range(true, None), &token).await;
    assert_eq!(status, 200, "dry run: {body}");
    let res: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        res["updated"], 3,
        "three of five readings fall in the range: {body}"
    );

    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) FROM readings WHERE {slot} AND is_flagged IS TRUE")
        )
        .await,
        0,
        "a dry run flags nothing"
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM reading_decisions").await,
        0,
        "a dry run records no decision"
    );
    assert_eq!(
        count(&db, &format!("SELECT sum(count)::bigint FROM readings_hourly WHERE {slot} AND bucket = '2025-06-15T10:00:00Z'")).await,
        5,
        "a dry run changes nothing the rollup serves: all five readings are still counted"
    );

    // The write reports the number the dry run promised.
    let (status, body) = patch_json_with_token(
        &app,
        "/api/readings/flag_range",
        &range(false, Some("spike")),
        &token,
    )
    .await;
    assert_eq!(status, 200, "flag: {body}");
    let res: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(res["updated"], 3);
    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) FROM readings WHERE {slot} AND is_flagged IS TRUE")
        )
        .await,
        3
    );

    // The unflag dry run counts only rows in the flagged state, and lifts none of them.
    let (status, body) = patch_json_with_token(
        &app,
        "/api/readings/unflag_range",
        &range(true, None),
        &token,
    )
    .await;
    assert_eq!(status, 200, "unflag dry run: {body}");
    let res: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(res["updated"], 3);
    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) FROM readings WHERE {slot} AND is_flagged IS TRUE")
        )
        .await,
        3,
        "an unflag dry run lifts no flag"
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM reading_decisions").await,
        3,
        "an unflag dry run records no decision"
    );

    cleanup_test_db(&db).await;
}
