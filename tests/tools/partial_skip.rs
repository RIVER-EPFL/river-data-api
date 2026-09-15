//! A set whose other outputs saved still reports the formula that did not run: the refusal
//! reaches the review queue under the reason the engine gave, rather than living only in
//! `tool_runs.context`.
//!
//! Formula engine only, so no R runs here.
//!
//! Run: cargo test --test tools partial_skip -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const CALCULATION: &str = "partial_probe";
const GROUP_ID: &str = "00000000-0000-4000-c000-000000000319";
const EVENT_TIME: &str = "2025-06-15T09:00:00Z";

async fn exec(db: &DatabaseConnection, sql: &str) {
    crate::common::exec(db, sql).await;
}

/// A calculation over a group holding temperature and oxygen, with two outputs: one reading the
/// temperature the visit holds, one reading the oxygen it does not.
async fn seed_calculation(db: &DatabaseConnection) -> (String, String) {
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name = '{CALCULATION}'"),
        format!("DELETE FROM tool_scripts WHERE name = '{CALCULATION}'"),
        format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal) \
             VALUES ('{GROUP_ID}', 'partial', 'Partial', 91)"
        ),
        format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{GROUP_ID}', '{GLOBAL_PARAM_TEMP_ID}', 1)"
        ),
        format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{GROUP_ID}', '{GLOBAL_PARAM_DO_ID}', 2)"
        ),
        format!(
            "INSERT INTO tool_scripts (name, label, engine, created_by) \
             VALUES ('{CALCULATION}', 'Partial probe', 'formula', 'test')"
        ),
    ] {
        exec(db, &sql).await;
    }
    let script_id: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT id::text AS id FROM tool_scripts WHERE name = '{CALCULATION}'"),
        ))
        .await
        .expect("query")
        .expect("the calculation")
        .try_get("", "id")
        .expect("id");
    (script_id, GROUP_ID.to_string())
}

/// The parameter a saved formula minted, declared at the site so the chain applies there. A
/// calculation mints its own output (Q191), so the slot is declared after the formula is added.
async fn declare_slot(db: &DatabaseConnection, code: &str) -> String {
    let parameter_id: Uuid = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT id FROM parameters WHERE lower(code) = lower('{code}')"),
        ))
        .await
        .expect("the catalog reads")
        .expect("the formula minted its output")
        .try_get("", "id")
        .expect("id");
    exec(
        db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, \
                 entry_mode, display_units) \
             VALUES (gen_random_uuid(), '{SITE1_ID}', '{parameter_id}', '{code}', 'derived', \
                 'tool', 'ratio')"
        ),
    )
    .await;
    parameter_id.to_string()
}

/// A visit holding temperature and nothing else.
async fn seed_visit(db: &DatabaseConnection) -> Uuid {
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
                 raw_value, measurement_type, collection_event_id) \
             VALUES ('{stream_id}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{EVENT_TIME}', 0, \
                 12.0, 'spot', '{event_id}')"
        ),
    ] {
        exec(db, &sql).await;
    }
    event_id
}

async fn hold_on(db: &DatabaseConnection, parameter_id: &str) -> Option<(String, String, String)> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT kind, status, COALESCE(expected->>'reason', '') AS reason \
               FROM replicate_audit_holds \
              WHERE site_id = '{SITE1_ID}' AND parameter_id = '{parameter_id}' \
                AND group_time = '{EVENT_TIME}'"
        ),
    ))
    .await
    .expect("query")
    .map(|r| {
        (
            r.try_get("", "kind").expect("kind"),
            r.try_get("", "status").expect("status"),
            r.try_get("", "reason").expect("reason"),
        )
    })
}

/// Scenario: a two-output set at a visit that holds one of the two inputs. One formula computes
/// and saves, the other reads a parameter the visit does not hold and is refused.
///
/// Expected behaviour: the output that saved carries no finding, and the one that did not carries
/// a pending `skipped_output` naming the engine's reason, counted in the run's report.
#[tokio::test]
#[serial]
async fn a_refused_output_of_a_set_that_saved_is_reported_rather_than_silent() {
    let f = crate::common::seeded_app().await;
    let db = f.db.clone();
    let (script_id, _) = seed_calculation(&db).await;
    let (status, text) = crate::common::save_formula_set(
        &f.app,
        &f.token,
        &script_id,
        serde_json::json!([
            { "code": "partial_computed", "name": "partial_computed", "units": "ratio",
              "formula": "DO_Temperature * 2", "ordinal": 1 },
            { "code": "partial_refused", "name": "partial_refused", "units": "ratio",
              "formula": "Dissolved_O2 * 2", "ordinal": 2 }
        ]),
    )
    .await;
    assert!((200..300).contains(&status), "the set ({status}): {text}");
    let computed = declare_slot(&db, "partial_computed").await;
    let refused = declare_slot(&db, "partial_refused").await;

    let event_id = seed_visit(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("a refused output is not a failed recompute");

    assert_eq!(outcome.tools_run, 1, "the set ran and saved");
    assert!(outcome.readings_written >= 1, "the computed output landed");
    assert_eq!(
        outcome.findings_raised, 1,
        "one output was refused: {:?}",
        outcome.skipped
    );

    assert_eq!(
        hold_on(&db, &computed).await,
        None,
        "the output that computed carries nothing"
    );
    let (kind, status, reason) = hold_on(&db, &refused)
        .await
        .expect("the refused output carries a finding");
    assert_eq!(kind, "skipped_output");
    assert_eq!(status, "pending");
    assert!(
        reason.contains("Dissolved_O2"),
        "the finding names why the formula did not run: {reason}"
    );
}
