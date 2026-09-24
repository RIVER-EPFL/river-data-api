//! A calculation runs only at a site it was added to (Q325, Q274). Holding every input it reads,
//! or an output slot the chain minted needing review (Q317), is not an assignment: the visit runs
//! nothing and mints nothing.
//!
//! Formula engine only, so no R runs here.
//!
//! Run: cargo test --test tools unassigned_site -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const CALCULATION: &str = "unassigned_probe";
const OUTPUT_CODE: &str = "unassigned_ratio";
const EVENT_TIME: &str = "2025-06-18T09:00:00Z";

async fn exec(db: &DatabaseConnection, sql: &str) {
    crate::common::exec(db, sql).await;
}

/// A calculation reading the temperature the site declares. Its output parameter is minted by the
/// formula save itself: a code the catalog already holds is refused (Q191).
async fn seed_calculation(db: &DatabaseConnection) -> String {
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name = '{CALCULATION}'"),
        format!("DELETE FROM tool_scripts WHERE name = '{CALCULATION}'"),
        format!(
            "INSERT INTO tool_scripts (name, label, engine, created_by) \
             VALUES ('{CALCULATION}', 'Unassigned probe', 'formula', 'test')"
        ),
    ] {
        exec(db, &sql).await;
    }
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT id::text AS id FROM tool_scripts WHERE name = '{CALCULATION}'"),
    ))
    .await
    .expect("query")
    .expect("the calculation")
    .try_get("", "id")
    .expect("id")
}

/// The catalog parameter a code names, once something has minted it.
async fn parameter_id(db: &DatabaseConnection, code: &str) -> Option<String> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT id::text AS id FROM parameters WHERE LOWER(code) = LOWER('{code}')"),
    ))
    .await
    .expect("query")
    .map(|r| r.try_get("", "id").expect("id"))
}

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
             VALUES ('{stream_id}', 'grab_sample', '{SITE1_ID}:{GLOBAL_PARAM_TEMP_ID}:mint', true)"
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

async fn slot(db: &DatabaseConnection, parameter_id: &str) -> Option<(bool, String, bool, String)> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT needs_review, entry_mode, COALESCE(is_public, false) AS is_public, cadence \
               FROM site_parameters \
              WHERE site_id = '{SITE1_ID}' AND parameter_id = '{parameter_id}'"
        ),
    ))
    .await
    .expect("query")
    .map(|r| {
        (
            r.try_get("", "needs_review").expect("needs_review"),
            r.try_get("", "entry_mode").expect("entry_mode"),
            r.try_get("", "is_public").expect("is_public"),
            r.try_get("", "cadence").expect("cadence"),
        )
    })
}

async fn add_formula(f: &crate::common::Fixture, script_id: &str) -> String {
    let (status, text) = crate::common::save_formula_set(
        &f.app,
        &f.token,
        script_id,
        serde_json::json!([{
            "code": OUTPUT_CODE,
            "name": OUTPUT_CODE,
            "units": "ratio",
            "formula": "DO_Temperature * 2",
            "ordinal": 1,
        }]),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "add the formula ({status}): {text}"
    );
    parameter_id(&f.db, OUTPUT_CODE)
        .await
        .expect("the formula save minted the output parameter")
}

async fn written_at(db: &DatabaseConnection, event_id: Uuid, parameter_id: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT COUNT(*)::bigint AS n FROM readings \
              WHERE collection_event_id = '{event_id}' AND parameter_id = '{parameter_id}'"
        ),
    ))
    .await
    .expect("query")
    .expect("a count row")
    .try_get("", "n")
    .expect("n")
}

/// Scenario: a site holds the calculation's input and no slot for its output, and nobody added
/// the calculation there.
///
/// Expected behaviour: the visit runs nothing, writes nothing and mints no output slot.
#[tokio::test]
#[serial]
async fn a_visit_at_a_site_holding_every_input_runs_nothing_and_mints_nothing() {
    let f = crate::common::seeded_app().await;
    let db = f.db.clone();
    let script_id = seed_calculation(&db).await;
    let output_id = add_formula(&f, &script_id).await;

    let event_id = seed_visit(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the run");

    assert_eq!(
        outcome.not_applicable,
        vec![CALCULATION.to_string()],
        "holding the input is not the calculation being added here"
    );
    assert_eq!(outcome.tools_run, 0);
    assert_eq!(written_at(&db, event_id, &output_id).await, 0);
    assert_eq!(slot(&db, &output_id).await, None, "the run mints no slot");
}

/// Scenario: the site holds the input and an output slot the chain minted needing review before
/// Q325, and nobody added the calculation there.
///
/// Expected behaviour: the minted slot is not an assignment (Q317), so the visit runs nothing, and
/// the slot is left as it was.
#[tokio::test]
#[serial]
async fn an_output_slot_waiting_on_review_does_not_run_the_calculation() {
    let f = crate::common::seeded_app().await;
    let db = f.db.clone();
    let script_id = seed_calculation(&db).await;
    let output_id = add_formula(&f, &script_id).await;
    exec(
        &db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, \
                                          is_active, is_public, needs_review, entry_mode, cadence) \
             VALUES (gen_random_uuid(), '{SITE1_ID}', '{output_id}', '{OUTPUT_CODE}', '', true, \
                     false, true, 'tool', 'low')"
        ),
    )
    .await;

    let event_id = seed_visit(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the run");

    assert_eq!(outcome.not_applicable, vec![CALCULATION.to_string()]);
    assert_eq!(outcome.tools_run, 0);
    assert_eq!(written_at(&db, event_id, &output_id).await, 0);
    assert_eq!(
        slot(&db, &output_id).await,
        Some((true, "tool".to_string(), false, "low".to_string())),
        "the minted slot still waits on review"
    );
}

/// A calculation reading a parameter the site does not declare stays out of that site.
#[tokio::test]
#[serial]
async fn a_calculation_whose_inputs_the_site_lacks_is_still_not_applicable() {
    let f = crate::common::seeded_app().await;
    let db = f.db.clone();
    let script_id = seed_calculation(&db).await;

    let unread = Uuid::new_v4();
    exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{unread}', 'Unheld_input', 'Unheld input', 'ppb', 'measurement')"
        ),
    )
    .await;
    let (status, text) = crate::common::save_formula_set(
        &f.app,
        &f.token,
        &script_id,
        serde_json::json!([{
            "code": OUTPUT_CODE,
            "name": OUTPUT_CODE,
            "units": "ratio",
            "formula": "Unheld_input * 2",
            "ordinal": 1,
        }]),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "add the formula ({status}): {text}"
    );
    let output_id = parameter_id(&db, OUTPUT_CODE)
        .await
        .expect("the formula save minted the output parameter");

    let event_id = seed_visit(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the run");

    assert_eq!(
        outcome.not_applicable,
        vec![CALCULATION.to_string()],
        "the site declares neither the input nor the output"
    );
    assert_eq!(slot(&db, &output_id).await, None);
}
