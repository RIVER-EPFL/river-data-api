//! M23: an output the script computes as NA is the portal's blanked column. The chain withdraws
//! the stored value rather than leaving a number standing that no run produced, reversibly, and
//! a person's ruling on the row is not overridden.
//!
//! Run: cargo test --test tools na_clears_output -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const TOOL: &str = "na_probe";
const EVENT_TIME: &str = "2025-06-15T09:00:00Z";

/// A tool reading the temperature at the visit and writing dissolved oxygen, which it computes as
/// NA whenever the temperature is negative.
const SCRIPT: &str = r"tool <- function(inputs, constants, curves) {
  list(out = if (inputs$t < 0) as.numeric(NA) else inputs$t * 2)
}";

fn manifest() -> serde_json::Value {
    json!({
        "label": "NA probe",
        "params": [{ "name": "t", "label": "T", "kind": "number", "required": true }],
        "event_inputs": [{ "param": "t", "parameter_code": "DO_Temperature" }],
        "outputs": [{ "key": "out", "label": "Out", "parameter_id": GLOBAL_PARAM_DO_ID }],
    })
}

async fn exec(db: &DatabaseConnection, sql: &str, values: Vec<sea_orm::Value>) {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\n{sql}"));
}

async fn install_tool(db: &DatabaseConnection) {
    for (sql, values) in [
        (
            "INSERT INTO tool_scripts (name, label, created_by) VALUES ($1, 'NA probe', 'test')",
            vec![TOOL.into()],
        ),
        (
            "INSERT INTO tool_script_versions
                 (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                  content_hash, created_by, validated_at)
             SELECT s.id, 1, $2, 'tool', $3::jsonb, '{}'::jsonb, md5($2), 'test', now()
             FROM tool_scripts s WHERE s.name = $1",
            vec![TOOL.into(), SCRIPT.into(), manifest().to_string().into()],
        ),
        (
            "UPDATE tool_scripts s SET active_version_id = v.id
             FROM tool_script_versions v
             WHERE v.tool_script_id = s.id AND s.name = $1",
            vec![TOOL.into()],
        ),
    ] {
        exec(db, sql, values).await;
    }
}

/// One visit holding a temperature the tool reads and a stored oxygen value it owns.
async fn seed_visit(db: &DatabaseConnection, temperature: f64) -> Uuid {
    let event_id = Uuid::new_v4();
    exec(
        db,
        "INSERT INTO collection_events (id, site_id, collected_at, source)
         VALUES ($1, $2::uuid, $3::timestamptz, 'manual')",
        vec![event_id.into(), SITE1_ID.into(), EVENT_TIME.into()],
    )
    .await;
    for (parameter_id, value) in [
        (GLOBAL_PARAM_TEMP_ID, temperature),
        (GLOBAL_PARAM_DO_ID, 99.0),
    ] {
        let stream_id = Uuid::new_v4();
        exec(
            db,
            "INSERT INTO data_streams (id, source_system, source_key, is_active)
             VALUES ($1, 'grab_sample', $2, true)",
            vec![stream_id.into(), format!("{SITE1_ID}:{parameter_id}").into()],
        )
        .await;
        exec(
            db,
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index,
                 raw_value, measurement_type, collection_event_id)
             VALUES ($1, $2::uuid, $3::uuid, $4::timestamptz, 0, $5, 'spot', $6)",
            vec![
                stream_id.into(),
                SITE1_ID.into(),
                parameter_id.into(),
                EVENT_TIME.into(),
                value.into(),
                event_id.into(),
            ],
        )
        .await;
    }
    event_id
}

struct Stored {
    value: Option<f64>,
    withdrawn: bool,
    chain_withdrawals: i64,
}

async fn stored_output(db: &DatabaseConnection) -> Stored {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT r.raw_value, r.withdrawn_at IS NOT NULL AS withdrawn,
                    (SELECT count(*) FROM reading_decisions d
                      WHERE d.stream_id = r.stream_id AND d.time = r.time
                        AND d.kind = 'withdraw' AND d.origin = 'chain') AS chain_withdrawals
             FROM readings r
             WHERE r.site_id = $1::uuid AND r.parameter_id = $2::uuid
               AND r.time = $3::timestamptz",
            [SITE1_ID.into(), GLOBAL_PARAM_DO_ID.into(), EVENT_TIME.into()],
        ))
        .await
        .unwrap()
        .expect("the output reading is still stored");
    Stored {
        value: row.try_get("", "raw_value").unwrap(),
        withdrawn: row.try_get("", "withdrawn").unwrap(),
        chain_withdrawals: row.try_get("", "chain_withdrawals").unwrap(),
    }
}

async fn setup(temperature: f64) -> (DatabaseConnection, river_db::common::AppState, Uuid) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    install_tool(&db).await;
    let event_id = seed_visit(&db, temperature).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    (db, state, event_id)
}

#[tokio::test]
#[serial]
async fn a_recompute_that_yields_na_withdraws_the_stored_output() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_recompute_that_yields_na_withdraws_the_stored_output",
    )
    .await
    {
        return;
    }
    let (db, state, event_id) = setup(-4.0).await;

    let outcome = river_db::routes::private::tools::chain::recompute_event(&state, event_id, "test")
        .await
        .expect("the recompute runs");
    assert_eq!(outcome.readings_withdrawn, 1, "the NA blanks the column");
    assert_eq!(
        outcome.readings_written, 0,
        "an NA writes no value of its own"
    );

    let stored = stored_output(&db).await;
    assert!(stored.withdrawn, "the stale value stops being served");
    assert_eq!(
        stored.value,
        Some(99.0),
        "nothing deletes: the number is still there behind a reversible stamp"
    );
    assert_eq!(
        stored.chain_withdrawals, 1,
        "the withdrawal is on the record as the chain's decision"
    );
}

#[tokio::test]
#[serial]
async fn a_computed_value_replaces_the_stored_one_and_withdraws_nothing() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_computed_value_replaces_the_stored_one_and_withdraws_nothing",
    )
    .await
    {
        return;
    }
    let (db, state, event_id) = setup(3.0).await;

    let outcome = river_db::routes::private::tools::chain::recompute_event(&state, event_id, "test")
        .await
        .expect("the recompute runs");
    assert_eq!(outcome.readings_withdrawn, 0, "a number clears nothing");

    let stored = stored_output(&db).await;
    assert!(!stored.withdrawn);
    assert_eq!(stored.value, Some(6.0), "the calculation owns the slot");
}

#[tokio::test]
#[serial]
async fn a_judged_output_keeps_its_value_when_the_recompute_yields_na() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_judged_output_keeps_its_value_when_the_recompute_yields_na",
    )
    .await
    {
        return;
    }
    let (db, state, event_id) = setup(-4.0).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reading_decisions (stream_id, time, replicate_index, kind, old, new, \
                 actor, origin) \
             SELECT r.stream_id, r.time, r.replicate_index, 'flag', '{{}}'::jsonb, \
                    '{{\"reason\": \"checked by hand\"}}'::jsonb, 'tester', 'manual' \
             FROM readings r \
             WHERE r.site_id = '{SITE1_ID}' AND r.parameter_id = '{GLOBAL_PARAM_DO_ID}' \
               AND r.time = '{EVENT_TIME}'"
        ),
    )
    .await;

    let outcome = river_db::routes::private::tools::chain::recompute_event(&state, event_id, "test")
        .await
        .expect("the recompute runs");
    assert_eq!(
        outcome.readings_withdrawn, 0,
        "a ruling on the row is not overridden by a calculation"
    );
    assert!(!stored_output(&db).await.withdrawn);
}
