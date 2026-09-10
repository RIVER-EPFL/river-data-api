//! Scenario: someone flags a reading that is in breach.
//!
//! Expected behaviour: a flagged continuous reading keeps alerting, because a flag says the value
//! is wrong and not that the instrument is fine; a spot instant is evaluated over its unflagged
//! replicates, so flagging one moves the evaluated value and flagging all of them leaves the
//! instant with nothing to evaluate. The continuous aggregates and every curated serving arm do
//! exclude flagged rows, which is why this arm is the one worth pinning.
//!
//! Run: cargo test --test alarms curation_and_evaluation -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const BREACH: &str = "2025-02-01T00:00:00Z";

async fn setup() -> (axum::Router, String, DatabaseConnection) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    crate::common::exec(&db, "DELETE FROM alarm_thresholds").await;
    crate::common::exec(&db, "DELETE FROM alarm_events").await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_max, alarm_max) \
             VALUES (gen_random_uuid(), '{}', NULL, 100, 500)",
            crate::common::GLOBAL_PARAM_TURB_ID
        ),
    )
    .await;
    (app, token, db)
}

/// A stream already feeding turbidity at site 1.
async fn turbidity_stream(db: &DatabaseConnection) -> Uuid {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT stream_id FROM readings WHERE site_id='{}' AND parameter_id='{}' LIMIT 1",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TURB_ID
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "stream_id")
    .unwrap()
}

async fn insert_breach(
    db: &DatabaseConnection,
    stream: Uuid,
    measurement_type: &str,
    replicate_index: i16,
    value: f64,
) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
             replicate_index, measurement_type) \
             VALUES ('{stream}', '{}', '{}', '{BREACH}', {value}, {replicate_index}, \
             '{measurement_type}') ON CONFLICT DO NOTHING",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TURB_ID
        ),
    )
    .await;
}

/// The severity `/alarms/active` reports for turbidity at site 1, if any.
async fn active_severity(app: &axum::Router, token: &str) -> Option<i64> {
    let (status, body) = crate::common::get_json_with_token(app, "/api/alarms/active", token).await;
    assert_eq!(status, 200, "active ({status}): {body}");
    body["alarms"]
        .as_array()
        .expect("alarms array")
        .iter()
        .find(|a| {
            a["parameter_id"].as_str() == Some(crate::common::GLOBAL_PARAM_TURB_ID)
                && a["site_id"].as_str() == Some(crate::common::SITE1_ID)
        })
        .and_then(|a| a["severity"].as_i64())
}

/// Flag the breach instant. A `replicate_index` scopes the write to spot rows by design
/// (`readings/flags.rs`), so the continuous case passes none.
async fn flag(app: &axum::Router, token: &str, indices: &[Option<i16>]) {
    let readings: Vec<serde_json::Value> = indices
        .iter()
        .map(|index| {
            let mut key = serde_json::json!({
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_TURB_ID,
                "time": BREACH,
            });
            if let Some(index) = index {
                key["replicate_index"] = serde_json::json!(index);
            }
            key
        })
        .collect();
    let (status, body) = crate::common::patch_json_with_token(
        app,
        "/api/readings/flag",
        &serde_json::json!({ "readings": readings, "reason": "curation and evaluation" }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "flag ({status}): {body}");
}

/// How many replicates of the breach instant are flagged, so a flag that matched nothing cannot
/// leave the assertions below vacuous.
async fn flagged_count(db: &DatabaseConnection) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT count(*)::bigint AS c FROM readings WHERE site_id='{}' \
             AND parameter_id='{}' AND time='{BREACH}' AND is_flagged",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TURB_ID
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "c")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn a_flagged_continuous_reading_keeps_alerting() {
    let (app, token, db) = setup().await;
    let stream = turbidity_stream(&db).await;
    insert_breach(&db, stream, "continuous", 0, 9999.0).await;

    assert_eq!(
        active_severity(&app, &token).await,
        Some(2),
        "the breach alarms before it is flagged"
    );

    flag(&app, &token, &[None]).await;
    assert_eq!(flagged_count(&db).await, 1, "the flag landed on the breach");

    assert_eq!(
        active_severity(&app, &token).await,
        Some(2),
        "a flag says the value is wrong, not that the instrument is in range"
    );
}

#[tokio::test]
#[serial]
async fn a_fully_flagged_spot_group_is_not_evaluated() {
    let (app, token, db) = setup().await;
    let stream = turbidity_stream(&db).await;
    for (index, value) in [(0i16, 9999.0), (1, 9998.0)] {
        insert_breach(&db, stream, "spot", index, value).await;
    }

    assert_eq!(
        active_severity(&app, &token).await,
        Some(2),
        "the spot instant alarms while a replicate is live"
    );

    flag(&app, &token, &[Some(0)]).await;
    assert_eq!(flagged_count(&db).await, 1, "the first flag landed");
    assert_eq!(
        active_severity(&app, &token).await,
        Some(2),
        "one flagged replicate moves the evaluated value, it does not hide the instant"
    );

    flag(&app, &token, &[Some(1)]).await;
    assert_eq!(flagged_count(&db).await, 2, "the whole group is flagged");
    assert_eq!(
        active_severity(&app, &token).await,
        None,
        "a group with no live replicate has nothing to evaluate"
    );
}
