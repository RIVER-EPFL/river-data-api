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
const OTHER_CODE: &str = "pending_probe_other";
const EVENT_TIME: &str = "2025-06-15T09:00:00Z";
const GROUP_ID: &str = "00000000-0000-4000-c000-0000000001a1";

/// A group holding the temperature the formula reads and the parameter it writes, with the
/// calculation bound to it, and the site carrying the output slot (Q98). `other` adds a second
/// output, returned second.
async fn install_calculation(
    db: &DatabaseConnection,
    app: &axum::Router,
    token: &str,
    other: bool,
) -> (String, String) {
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
            "INSERT INTO tool_scripts (name, label, engine, created_by) \
             VALUES ('{CALCULATION}', 'Pending probe', 'formula', 'test')"
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
    let mut formulas = vec![json!({
        "code": OUTPUT_CODE,
        "name": OUTPUT_CODE,
        "units": "ratio",
        "formula": "DO_Temperature * 2",
        "ordinal": 1,
    })];
    if other {
        formulas.push(json!({
            "code": OTHER_CODE,
            "name": OTHER_CODE,
            "units": "ratio",
            "formula": "DO_Temperature * 3",
            "ordinal": 2,
        }));
    }
    let (status, body) =
        crate::common::save_formula_set(app, token, &script_id, json!(formulas)).await;
    assert!(
        (200..300).contains(&status),
        "the formula mints a version ({status}): {body}"
    );

    // The formula mints its output (Q191) and a person puts it in the group (Q189), so the group
    // carries it only once both have happened.
    let codes = if other {
        &[OUTPUT_CODE, OTHER_CODE][..]
    } else {
        &[OUTPUT_CODE][..]
    };
    let mut ids = Vec::new();
    for (ordinal, code) in codes.iter().enumerate() {
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT id FROM parameters WHERE lower(code) = lower('{code}')"),
            ))
            .await
            .expect("query")
            .expect("the formula minted its output");
        let id = row.try_get::<Uuid>("", "id").expect("id").to_string();
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
                 VALUES (gen_random_uuid(), '{GROUP_ID}', '{id}', {})",
                ordinal + 2
            ),
        )
        .await;
        ids.push(id);
    }

    let (status, applied) = crate::common::post_json_with_token(
        app,
        &format!("/api/sites/{SITE1_ID}/parameter_groups"),
        &json!({ "group_id": GROUP_ID }),
        token,
    )
    .await;
    assert_eq!(status, 200, "the site carries the slots: {applied}");
    let other_id = ids.get(1).cloned().unwrap_or_default();
    (ids.swap_remove(0), other_id)
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

async fn pending_entry_holds(db: &DatabaseConnection, parameter_id: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT count(*) AS c FROM replicate_audit_holds \
              WHERE kind = 'unverified_entry' AND status = 'pending' AND site_id = '{SITE1_ID}' \
                AND parameter_id = '{parameter_id}' AND group_time = '{EVENT_TIME}'"
        ),
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get("", "c")
    .expect("count")
}

#[tokio::test]
#[serial]
async fn an_output_computed_from_a_pending_measurement_is_pending_itself() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    let (output_id, _) = install_calculation(&db, &app, &token, false).await;
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
    assert_eq!(
        pending_entry_holds(&db, &output_id).await,
        1,
        "the review queue lists the pending output, so a manager can rule on it"
    );
}

/// Scenario: the output slot is detached at the visit, carrying a hand-entered value an intern
/// entered, and the chain runs over the visit.
///
/// Expected behaviour: the run writes its other output and nothing at the detached slot, and
/// leaves the entry's review-queue row there open.
#[tokio::test]
#[serial]
async fn a_pending_entry_at_a_slot_the_run_does_not_own_stays_in_the_queue() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    let (output_id, other_id) = install_calculation(&db, &app, &token, true).await;
    let event_id = seed_pending_visit(&db).await;
    let manual = Uuid::new_v4();
    for sql in [
        format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{manual}', 'grab_sample', '{SITE1_ID}:{output_id}', true)"
        ),
        format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                 raw_value, measurement_type, collection_event_id, unverified) \
             VALUES ('{manual}', '{SITE1_ID}', '{output_id}', '{EVENT_TIME}', 0, 3.0, 'spot', \
                     '{event_id}', true)"
        ),
        format!(
            "INSERT INTO reading_decisions (stream_id, time, replicate_index, kind, actor, origin) \
             VALUES ('{manual}', '{EVENT_TIME}', 0, 'detach', 'test', 'manual')"
        ),
        format!(
            "INSERT INTO replicate_audit_holds (site_id, parameter_id, group_time, kind, expected, \
                 computed, delta, status) \
             VALUES ('{SITE1_ID}', '{output_id}', '{EVENT_TIME}', 'unverified_entry', \
                     '{{\"state\": \"verified\"}}', '{{\"state\": \"unverified\"}}', '{{}}', 'pending')"
        ),
    ] {
        crate::common::exec(&db, &sql).await;
    }

    river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
        .await
        .expect("the recompute runs");

    assert_eq!(
        output_rows(&db, &other_id).await,
        vec![(12.0, true)],
        "4 tripled"
    );
    assert_eq!(output_rows(&db, &output_id).await, vec![(3.0, true)]);
    assert_eq!(
        pending_entry_holds(&db, &output_id).await,
        1,
        "the hand-entered value's review-queue row survives a run that did not write it"
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
    let (output_id, _) = install_calculation(&db, &app, &token, false).await;
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

/// Scenario: a manager rejects the intern's entry an output was computed from.
///
/// Expected behaviour: the ruling queues the visit's calculations as any curation decision does,
/// and the audit reports the output it can no longer compute rather than passing over it.
#[tokio::test]
#[serial]
async fn rejecting_an_input_requeues_the_visit_and_the_audit_reports_its_output() {
    use river_db::routes::private::tools::flows;
    use river_db::routes::private::tools::models::AuditCounts;
    use river_db::routes::private::tools::service;

    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    let (output_id, _) = install_calculation(&db, &app, &token, false).await;
    let event_id = seed_pending_visit(&db).await;
    flows::recompute_event(&state, event_id, "test")
        .await
        .expect("the recompute runs");

    let hold = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO replicate_audit_holds (id, site_id, parameter_id, group_time, kind, \
                 expected, computed, delta, status) \
             VALUES ('{hold}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{EVENT_TIME}', \
                     'unverified_entry', '{{}}', '{{}}', '{{}}', 'pending')"
        ),
    )
    .await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/replicate_audit_holds/{hold}/resolve"),
        &json!({ "mode": "reject" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let queued: i64 = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*) AS c FROM reprocessing_jobs WHERE trigger_type = 'event_recompute'",
        ))
        .await
        .expect("query")
        .expect("a row")
        .try_get("", "c")
        .expect("count");
    assert_eq!(queued, 1, "the ruling queues the visit's calculations");

    let event = flows::load_event(&db, event_id).await.expect("event");
    let tools = service::list_active_tools(&db).await.expect("active tools");
    let catalog = service::load_parameter_catalog(&db, tools.iter().map(|t| &t.manifest))
        .await
        .expect("catalog");
    let order = flows::dependency_order(&tools, &catalog).expect("an order");
    let mut counts = AuditCounts {
        events_audited: 0,
        missing: 0,
        stale: 0,
        skipped: 0,
        superseded: 0,
    };
    flows::audit_event(&state, &event, &tools, &catalog, &order, &mut counts)
        .await
        .expect("the audit runs");
    assert_eq!(counts.skipped, 1, "the stored output is reported");
    let findings: i64 = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT count(*) AS c FROM replicate_audit_holds \
                  WHERE kind = 'skipped_output' AND status = 'pending' \
                    AND parameter_id = '{output_id}' AND group_time = '{EVENT_TIME}'"
            ),
        ))
        .await
        .expect("query")
        .expect("a row")
        .try_get("", "c")
        .expect("count");
    assert_eq!(findings, 1, "one finding at the output slot");
}
