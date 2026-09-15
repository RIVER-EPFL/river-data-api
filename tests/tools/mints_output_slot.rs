//! A site that declares what a calculation reads gets the column the calculation publishes: the
//! run mints the output slot rather than reporting itself not applicable there (Q193). The slot
//! arrives needing review, so a manager confirms it from the site's Parameters tab.
//!
//! Formula engine only, so no R runs here.
//!
//! Run: cargo test --test tools mints_output_slot -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const CALCULATION: &str = "minting_probe";
const OUTPUT_CODE: &str = "minted_ratio";
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
             VALUES ('{CALCULATION}', 'Minting probe', 'formula', 'test')"
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

async fn slot(db: &DatabaseConnection, parameter_id: &str) -> Option<(bool, String, bool)> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT needs_review, entry_mode, COALESCE(is_public, false) AS is_public \
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
        )
    })
}

/// Scenario: a site holds the calculation's input and no slot for its output, which is every
/// CNET site before somebody adds 19 slots by hand.
///
/// Expected behaviour: the chain runs there, the value lands, and the output slot is minted
/// carrying `needs_review` so a manager confirms the column.
#[tokio::test]
#[serial]
async fn a_run_publishing_where_the_site_declares_its_inputs_mints_the_output_slot() {
    let f = crate::common::seeded_app().await;
    let db = f.db.clone();
    let script_id = seed_calculation(&db).await;

    let (status, text) = crate::common::save_formula_set(
        &f.app,
        &f.token,
        &script_id,
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
    let output_id = parameter_id(&db, OUTPUT_CODE)
        .await
        .expect("the formula save minted the output parameter");

    assert_eq!(
        slot(&db, &output_id).await,
        None,
        "the site declares no slot for the output before the run"
    );

    let event_id = seed_visit(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the run");

    assert_eq!(
        outcome.not_applicable,
        Vec::<String>::new(),
        "the site declares what the calculation reads, so it applies here"
    );
    assert_eq!(
        outcome.tools_run, 1,
        "the calculation ran: {:?}",
        outcome.skipped
    );
    assert_eq!(
        outcome.slots_minted, 1,
        "the output slot was minted by the run"
    );
    assert!(outcome.readings_written >= 1, "the computed value landed");

    let (needs_review, entry_mode, is_public) = slot(&db, &output_id)
        .await
        .expect("the run minted the output slot");
    assert!(
        needs_review,
        "the slot waits for a manager to confirm the column"
    );
    assert_eq!(
        entry_mode, "tool",
        "the slot computes rather than being typed into"
    );
    assert!(
        !is_public,
        "a minted slot is not published until somebody says so"
    );
}

/// A calculation reading a parameter the site does not declare stays out of that site: the
/// narrowing is of which declaration counts, not of whether one is needed.
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
    assert_eq!(outcome.slots_minted, 0);
    assert_eq!(slot(&db, &output_id).await, None);
}
