//! M62: a value the chain computes from a measurement still awaiting verification is pending
//! itself, so nothing derived from an intern's entry is counted in the statistics or served
//! before a manager has ruled on the entry.
//!
//! The calculation is a formula one, so the chain runs with no tool runner.
//!
//! Run: cargo test --test tools pending_inputs_inherit -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const CALCULATION: &str = "pending_probe";
const OUTPUT_CODE: &str = "pending_probe_out";
const EVENT_TIME: &str = "2025-06-15T09:00:00Z";
const GROUP_ID: &str = "00000000-0000-4000-c000-0000000001a1";

/// A group holding the temperature the formula reads and the parameter it writes, with the
/// calculation bound to it, and the site carrying the output slot (Q98).
async fn install_calculation(db: &DatabaseConnection, app: &axum::Router, token: &str) -> String {
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name = '{CALCULATION}'"),
        format!("DELETE FROM tool_scripts WHERE name = '{CALCULATION}'"),
        format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal) \
             VALUES ('{GROUP_ID}', 'pending', 'Pending', 1)"
        ),
        format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{GROUP_ID}', '{GLOBAL_PARAM_TEMP_ID}', 1)"
        ),
        format!(
            "INSERT INTO tool_scripts (name, label, engine, parameter_group_id, created_by) \
             VALUES ('{CALCULATION}', 'Pending probe', 'formula', '{GROUP_ID}', 'test')"
        ),
    ] {
        crate::common::exec(db, &sql).await;
    }

    let output_id = Uuid::new_v4().to_string();
    for sql in [
        format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{output_id}', '{OUTPUT_CODE}', '{OUTPUT_CODE}', 'ratio', 'measurement')"
        ),
        format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{GROUP_ID}', '{output_id}', 2)"
        ),
    ] {
        crate::common::exec(db, &sql).await;
    }

    let script_id = {
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT id FROM tool_scripts WHERE name = '{CALCULATION}'"),
            ))
            .await
            .expect("query")
            .expect("the calculation");
        row.try_get::<Uuid>("", "id").expect("id").to_string()
    };
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/derived_parameters",
        &json!({
            "code": OUTPUT_CODE,
            "name": OUTPUT_CODE,
            "units": "ratio",
            "formula": "DO_Temperature * 2",
            "tool_script_id": script_id,
            "ordinal": 1,
        }),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the formula mints a version ({status}): {body}"
    );

    let (status, applied) = crate::common::post_json_with_token(
        app,
        &format!("/api/sites/{SITE1_ID}/parameter_groups"),
        &json!({ "group_id": GROUP_ID }),
        token,
    )
    .await;
    assert_eq!(status, 200, "the site carries the slots: {applied}");
    output_id
}

/// One visit holding a temperature an intern entered, still awaiting verification.
async fn seed_pending_visit(db: &DatabaseConnection) -> Uuid {
    let event_id = Uuid::new_v4();
    let stream_id = Uuid::new_v4();
    for sql in [
        format!(
            "INSERT INTO collection_events (id, site_id, collected_at, source) \
             VALUES ('{event_id}', '{SITE1_ID}', '{EVENT_TIME}', 'manual')"
        ),
        format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream_id}', 'grab_sample', '{SITE1_ID}:{GLOBAL_PARAM_TEMP_ID}', true)"
        ),
        format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                 raw_value, measurement_type, collection_event_id, unverified) \
             VALUES ('{stream_id}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{EVENT_TIME}', 0, \
                     4.0, 'spot', '{event_id}', true)"
        ),
    ] {
        crate::common::exec(db, &sql).await;
    }
    event_id
}

async fn output_rows(db: &DatabaseConnection, output_id: &str) -> Vec<(f64, bool)> {
    db.query_all_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT raw_value, unverified FROM readings \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{output_id}' \
               AND time = '{EVENT_TIME}' ORDER BY replicate_index"
        ),
    ))
    .await
    .expect("query")
    .iter()
    .map(|r| {
        (
            r.try_get("", "raw_value").expect("raw_value"),
            r.try_get("", "unverified").expect("unverified"),
        )
    })
    .collect()
}

#[tokio::test]
#[serial]
async fn an_output_computed_from_a_pending_measurement_is_pending_itself() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    let output_id = install_calculation(&db, &app, &token).await;
    let event_id = seed_pending_visit(&db).await;

    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the recompute runs");
    assert_eq!(outcome.readings_written, 1, "the formula writes its output");

    let rows = output_rows(&db, &output_id).await;
    assert_eq!(rows.len(), 1, "one output reading");
    assert!((rows[0].0 - 8.0).abs() < 1e-9, "4 doubled");
    assert!(
        rows[0].1,
        "the output inherits the state of the measurement it was computed from"
    );

    let recorded = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT count(*) AS c FROM reading_decisions d \
                   JOIN readings r ON r.stream_id = d.stream_id AND r.time = d.time \
                  WHERE d.kind = 'unverified_entry' AND r.parameter_id = '{output_id}' \
                    AND r.time = '{EVENT_TIME}'"
            ),
        ))
        .await
        .expect("query")
        .expect("a row");
    let recorded: i64 = recorded.try_get("", "c").expect("count");
    assert_eq!(
        recorded, 1,
        "the ledger carries the pending entry, not only the column"
    );
}

#[tokio::test]
#[serial]
async fn an_output_computed_from_a_verified_measurement_is_served() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    let output_id = install_calculation(&db, &app, &token).await;
    let event_id = seed_pending_visit(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET unverified = false WHERE site_id = '{SITE1_ID}' \
               AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' AND time = '{EVENT_TIME}'"
        ),
    )
    .await;

    river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
        .await
        .expect("the recompute runs");

    let rows = output_rows(&db, &output_id).await;
    assert_eq!(rows.len(), 1, "one output reading");
    assert!(
        !rows[0].1,
        "nothing at the visit is pending, so the output is not either"
    );
}
