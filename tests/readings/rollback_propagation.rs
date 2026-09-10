//! Scenario: an edit is rolled back, and the surfaces the forward edit updated have to follow it.
//!
//! Expected behaviour: the inverse does what the apply did. Restoring a flagged continuous reading
//! refreshes the rollups that were refreshed without it, and restoring a value at a manual visit
//! queues the calculations that read it.

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, PARAM_S1_TEMP_ID, SITE1_ID};

const BUCKET: &str = "2025-06-15T10:00:00Z";
const SPOT_AT: &str = "2025-06-15T11:00:00Z";

async fn hourly_count(db: &DatabaseConnection) -> Option<i64> {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT count FROM readings_hourly \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
                   AND bucket = '{BUCKET}'"
            ),
        ))
        .await
        .expect("query readings_hourly")?;
    Some(row.try_get("", "count").expect("count column"))
}

async fn edit(
    app: &axum::Router,
    token: &str,
    selection: &serde_json::Value,
    decision: &serde_json::Value,
) -> serde_json::Value {
    let (status, preview) = crate::common::post_json_parse_with_token(
        app,
        "/api/readings/edits/preview",
        &json!({ "selection": selection, "decision": decision }),
        token,
    )
    .await;
    assert_eq!(status, 200, "preview: {preview}");
    let (status, committed) = crate::common::post_json_parse_with_token(
        app,
        "/api/readings/edits",
        &json!({
            "selection": selection,
            "decision": decision,
            "preview_id": preview["preview_id"].as_str().expect("preview id"),
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "commit: {committed}");
    committed
}

#[tokio::test]
#[serial]
async fn rolling_back_a_flag_puts_the_reading_back_in_the_rollup() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        &db,
        "rollback-temp",
        PARAM_S1_TEMP_ID,
    )
    .await;
    for (i, v) in [10.0, 20.0, 30.0, 40.0, 50.0].iter().enumerate() {
        crate::common::exec(
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

    let selection = json!({
        "keys": [{ "stream_id": stream, "time": "2025-06-15T10:04:00Z", "replicate_index": 0 }]
    });
    let committed = edit(
        &app,
        &token,
        &selection,
        &json!({ "kind": "flag", "reason": "spike" }),
    )
    .await;
    assert_eq!(
        hourly_count(&db).await,
        Some(4),
        "the flag's refresh drops the flagged reading from the bucket"
    );

    let decision_id = committed["decision_ids"][0]
        .as_str()
        .expect("decision id")
        .to_string();
    let (status, rolled) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/readings/edits/{decision_id}/rollback"),
        &json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "rollback: {rolled}");

    assert_eq!(
        hourly_count(&db).await,
        Some(5),
        "the rollback refreshes the same buckets the flag did"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// An enabled calculation whose only event input is the site's temperature parameter.
async fn install_calculation(db: &DatabaseConnection) {
    let code: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT code FROM parameters WHERE id = '{GLOBAL_PARAM_TEMP_ID}'"),
        ))
        .await
        .expect("code query")
        .expect("the seeded parameter")
        .try_get("", "code")
        .expect("code column");
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = 'rollbackrecompute'",
        "DELETE FROM tool_script_versions v USING tool_scripts s \
          WHERE v.tool_script_id = s.id AND s.name = 'rollbackrecompute'",
        "DELETE FROM tool_scripts WHERE name = 'rollbackrecompute'",
    ] {
        crate::common::exec(db, sql).await;
    }
    let manifest = serde_json::json!({
        "label": "Rollback recompute",
        "params": [{ "name": "t", "label": "T", "kind": "number", "required": true }],
        "event_inputs": [{ "param": "t", "parameter_code": code }],
        "outputs": [{ "key": "out", "label": "O", "suggested_parameter_code": "RollbackRecomputeOut" }],
    });
    for statement in [
        Statement::from_string(
            DatabaseBackend::Postgres,
            "INSERT INTO tool_scripts (name, label, created_by) \
             VALUES ('rollbackrecompute', 'Rollback recompute', 'test')"
                .to_string(),
        ),
        Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            r"INSERT INTO tool_script_versions
                  (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                   content_hash, created_by, validated_at)
              SELECT s.id, 1, $1, 'tool', $2::jsonb, '{}'::jsonb, md5($1), 'test', now()
              FROM tool_scripts s WHERE s.name = 'rollbackrecompute'",
            [
                "tool <- function(inputs, constants, curves) list(out = 1)".into(),
                manifest.to_string().into(),
            ],
        ),
        Statement::from_string(
            DatabaseBackend::Postgres,
            r"UPDATE tool_scripts s SET active_version_id = v.id
              FROM tool_script_versions v
              WHERE v.tool_script_id = s.id AND s.name = 'rollbackrecompute'"
                .to_string(),
        ),
    ] {
        db.execute_raw(statement)
            .await
            .expect("calculation installed");
    }
}

async fn recomputes_queued(db: &DatabaseConnection, event: Uuid) -> i64 {
    crate::common::e2e::count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs \
             WHERE trigger_type = 'event_recompute' AND trigger_id = '{event}'"
        ),
    )
    .await
}

#[tokio::test]
#[serial]
async fn rolling_back_a_withdrawal_recomputes_the_visit_that_reads_it() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    install_calculation(&db).await;

    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        &db,
        "rollback-visit",
        PARAM_S1_TEMP_ID,
    )
    .await;
    let event = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO collection_events (id, site_id, collected_at, source) \
             VALUES ('{event}', '{SITE1_ID}', '{SPOT_AT}', 'manual')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings \
             (stream_id, site_id, parameter_id, time, raw_value, replicate_index, \
              measurement_type, collection_event_id) \
             VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{SPOT_AT}', 12.5, 0, \
                     'spot', '{event}')"
        ),
    )
    .await;

    let selection =
        json!({ "keys": [{ "stream_id": stream, "time": SPOT_AT, "replicate_index": 0 }] });
    let committed = edit(
        &app,
        &token,
        &selection,
        &json!({ "kind": "withdraw", "reason": "entered twice" }),
    )
    .await;
    let after_withdraw = recomputes_queued(&db, event).await;
    assert!(
        after_withdraw >= 1,
        "the withdrawal itself queues the visit's calculations"
    );

    // The queued job is claimed and finished before the rollback, so a second enqueue is a new row
    // rather than the dedupe key coalescing onto the first.
    crate::common::exec(
        &db,
        &format!(
            "UPDATE reprocessing_jobs SET status = 'completed', dedupe_key = NULL \
             WHERE trigger_type = 'event_recompute' AND trigger_id = '{event}'"
        ),
    )
    .await;

    let decision_id = committed["decision_ids"][0]
        .as_str()
        .expect("decision id")
        .to_string();
    let (status, rolled) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/readings/edits/{decision_id}/rollback"),
        &json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "rollback: {rolled}");

    assert_eq!(
        recomputes_queued(&db, event).await,
        after_withdraw + 1,
        "restoring the input queues the calculations that read it"
    );

    crate::common::cleanup_test_db(&db).await;
}
