//! Applying a calculation at a site from its own page: the site's slots are checked against what
//! the calculation reads before anything is written, and the output slots it lacks are created
//! declared rather than flagged for review (M308). Applying is the only thing that runs it there
//! (Q325).
//!
//! Formula engine only, so no R runs here.
//!
//! Run: cargo test --test tools apply_calculation -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const CALCULATION: &str = "apply_probe";
const OUTPUT_CODE: &str = "applied_ratio";
const EVENT_TIME: &str = "2025-07-02T09:00:00Z";

async fn exec(db: &DatabaseConnection, sql: &str) {
    crate::common::exec(db, sql).await;
}

async fn seed_calculation(db: &DatabaseConnection) -> String {
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name = '{CALCULATION}'"),
        format!("DELETE FROM tool_scripts WHERE name = '{CALCULATION}'"),
        format!(
            "INSERT INTO tool_scripts (name, label, engine, created_by) \
             VALUES ('{CALCULATION}', 'Apply probe', 'formula', 'test')"
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

async fn parameter_id(db: &DatabaseConnection, code: &str) -> Option<String> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT id::text AS id FROM parameters WHERE LOWER(code) = LOWER('{code}')"),
    ))
    .await
    .expect("query")
    .map(|r| r.try_get("", "id").expect("id"))
}

async fn slot(db: &DatabaseConnection, parameter_id: &str) -> Option<(bool, String, String)> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT needs_review, entry_mode, cadence FROM site_parameters \
              WHERE site_id = '{SITE1_ID}' AND parameter_id = '{parameter_id}'"
        ),
    ))
    .await
    .expect("query")
    .map(|r| {
        (
            r.try_get("", "needs_review").expect("needs_review"),
            r.try_get("", "entry_mode").expect("entry_mode"),
            r.try_get("", "cadence").expect("cadence"),
        )
    })
}

async fn slot_count(db: &DatabaseConnection, parameter_id: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT COUNT(*)::bigint AS n FROM site_parameters \
              WHERE site_id = '{SITE1_ID}' AND parameter_id = '{parameter_id}'"
        ),
    ))
    .await
    .expect("query")
    .expect("a count row")
    .try_get("", "n")
    .expect("n")
}

async fn add_formula(f: &crate::common::Fixture, script_id: &str, formula: &str) -> (u16, String) {
    crate::common::save_formula_set(
        &f.app,
        &f.token,
        script_id,
        serde_json::json!([{
            "code": OUTPUT_CODE,
            "name": OUTPUT_CODE,
            "units": "ratio",
            "formula": formula,
            "ordinal": 1,
        }]),
    )
    .await
}

async fn apply(
    f: &crate::common::Fixture,
    script_id: &str,
    dry_run: bool,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        &f.app,
        &format!("/api/sites/{SITE1_ID}/calculations"),
        &serde_json::json!({ "calculation_id": script_id, "dry_run": dry_run }),
        &f.token,
    )
    .await
}

fn codes(list: &serde_json::Value) -> Vec<String> {
    list.as_array()
        .expect("a list of slots")
        .iter()
        .map(|s| s["parameter_code"].as_str().expect("code").to_string())
        .collect()
}

/// Scenario: the calculation reads the temperature the site declares and writes a ratio it does
/// not, which is every CNET site before somebody adds the output columns by hand.
///
/// Expected behaviour: the dry run reports the input present and the output to add and writes
/// nothing; the apply mints that one slot declared, not flagged for review; a second apply mints
/// nothing; and the chain then runs there minting nothing of its own.
#[tokio::test]
#[serial]
async fn applying_a_calculation_mints_its_output_slots_and_leaves_the_second_apply_with_nothing() {
    let f = crate::common::seeded_app().await;
    let db = f.db.clone();
    let script_id = seed_calculation(&db).await;
    // The scenario is a field parameter entered at a visit, so the input slot is the visit arm's;
    // the seed declares temperature on the stream arm.
    exec(
        &db,
        &format!(
            "UPDATE site_parameters SET cadence = 'low' \
              WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}'"
        ),
    )
    .await;

    let (status, text) = add_formula(&f, &script_id, "DO_Temperature * 2").await;
    assert!(
        (200..300).contains(&status),
        "add the formula ({status}): {text}"
    );
    let output_id = parameter_id(&db, OUTPUT_CODE)
        .await
        .expect("the formula save minted the output parameter");

    let (status, dry) = apply(&f, &script_id, true).await;
    assert_eq!(status, 200, "the dry run: {dry}");
    assert_eq!(codes(&dry["inputs_missing"]), Vec::<String>::new());
    assert_eq!(
        codes(&dry["outputs_created"]),
        vec![OUTPUT_CODE.to_string()]
    );
    assert!(
        !codes(&dry["inputs_present"]).is_empty(),
        "the input the site declares is reported: {dry}"
    );
    assert_eq!(
        slot(&db, &output_id).await,
        None,
        "a dry run writes no slot: {dry}"
    );

    let (status, applied) = apply(&f, &script_id, false).await;
    assert_eq!(status, 200, "the apply: {applied}");
    assert_eq!(
        codes(&applied["outputs_created"]),
        vec![OUTPUT_CODE.to_string()]
    );

    let (needs_review, entry_mode, cadence) = slot(&db, &output_id)
        .await
        .expect("the apply minted the output slot");
    assert!(
        !needs_review,
        "a person applied it against a checked declaration, so there is nothing to confirm"
    );
    assert_eq!(
        entry_mode, "tool",
        "the slot computes rather than being typed into"
    );
    assert_eq!(
        cadence, "low",
        "the input is a visit slot at this site, so the output is the visit arm's"
    );

    let (status, again) = apply(&f, &script_id, false).await;
    assert_eq!(status, 200, "applying twice: {again}");
    assert_eq!(codes(&again["outputs_created"]), Vec::<String>::new());
    assert_eq!(
        codes(&again["outputs_existing"]),
        vec![OUTPUT_CODE.to_string()]
    );
    assert_eq!(
        slot_count(&db, &output_id).await,
        1,
        "the second apply created no second slot"
    );

    let event_id = Uuid::new_v4();
    let stream_id = Uuid::new_v4();
    for sql in [
        format!(
            "INSERT INTO collection_events (id, site_id, collected_at, source) \
             VALUES ('{event_id}', '{SITE1_ID}', '{EVENT_TIME}', 'manual')"
        ),
        format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream_id}', 'grab_sample', '{SITE1_ID}:{GLOBAL_PARAM_TEMP_ID}:apply', true)"
        ),
        format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                 raw_value, measurement_type, collection_event_id) \
             VALUES ('{stream_id}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{EVENT_TIME}', 0, \
                 12.0, 'spot', '{event_id}')"
        ),
    ] {
        exec(&db, &sql).await;
    }
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the run");
    assert_eq!(
        outcome.not_applicable,
        Vec::<String>::new(),
        "the calculation was applied here"
    );
    assert_eq!(
        slot_count(&db, &output_id).await,
        1,
        "the apply already declared the output, so the run adds no slot"
    );
    assert!(outcome.readings_written >= 1, "the computed value landed");
}

/// Scenario: the site holds an output slot the chain minted needing review before Q325, and a
/// manager applies the calculation there.
///
/// Expected behaviour: the apply confirms that slot rather than creating a second one, which is
/// what makes the calculation run at the site (Q317).
#[tokio::test]
#[serial]
async fn applying_a_calculation_confirms_an_output_slot_waiting_on_review() {
    let f = crate::common::seeded_app().await;
    let db = f.db.clone();
    let script_id = seed_calculation(&db).await;
    exec(
        &db,
        &format!(
            "UPDATE site_parameters SET cadence = 'low' \
              WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}'"
        ),
    )
    .await;
    let (status, text) = add_formula(&f, &script_id, "DO_Temperature * 2").await;
    assert!((200..300).contains(&status), "({status}): {text}");
    let output_id = parameter_id(&db, OUTPUT_CODE).await.expect("the output");
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

    let (status, applied) = apply(&f, &script_id, false).await;
    assert_eq!(status, 200, "{applied}");
    assert_eq!(codes(&applied["outputs_created"]), Vec::<String>::new());
    assert_eq!(
        codes(&applied["outputs_existing"]),
        vec![OUTPUT_CODE.to_string()]
    );
    assert_eq!(slot_count(&db, &output_id).await, 1);
    assert!(
        !slot(&db, &output_id).await.expect("the slot").0,
        "the apply confirmed the minted slot"
    );
}

/// Scenario: the calculation reads a parameter the site does not measure.
///
/// Expected behaviour: the apply is refused naming that parameter, and no slot is written, so the
/// answer arrives before the run rather than as a `not_applicable` line in a job log.
#[tokio::test]
#[serial]
async fn applying_a_calculation_the_site_cannot_feed_is_refused_naming_the_missing_input() {
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
    let (status, text) = add_formula(&f, &script_id, "Unheld_input * 2").await;
    assert!(
        (200..300).contains(&status),
        "add the formula ({status}): {text}"
    );
    let output_id = parameter_id(&db, OUTPUT_CODE)
        .await
        .expect("the formula save minted the output parameter");

    let (status, dry) = apply(&f, &script_id, true).await;
    assert_eq!(status, 200, "the dry run still answers: {dry}");
    assert_eq!(
        codes(&dry["inputs_missing"]),
        vec!["Unheld_input".to_string()],
        "the dry run names what is missing: {dry}"
    );

    let (status, refusal) = apply(&f, &script_id, false).await;
    assert_eq!(status, 400, "the apply is refused: {refusal}");
    assert!(
        refusal.to_string().contains("Unheld_input"),
        "the refusal names the parameter the site does not measure: {refusal}"
    );
    assert_eq!(
        slot(&db, &output_id).await,
        None,
        "a refused apply writes no slot"
    );
}

/// Scenario: the calculation reads the oxygen the site streams and a lab value it records at
/// visits.
///
/// Expected behaviour: the output takes the visit arm. Every input decides the arm, since a source
/// is read at the instant computed and the lab value exists only at visits (Q252).
#[tokio::test]
#[serial]
async fn a_visit_input_puts_the_output_on_the_visit_arm() {
    let f = crate::common::seeded_app().await;
    let db = f.db.clone();
    let script_id = seed_calculation(&db).await;

    // The lab value: a catalog parameter the site records at visits.
    let lab = Uuid::new_v4();
    exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, code, name, category) \
             VALUES ('{lab}', 'Lab_value', 'Lab value', 'measurement')"
        ),
    )
    .await;
    exec(
        &db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, \
                                          is_active, entry_mode, cadence) \
             VALUES (gen_random_uuid(), '{SITE1_ID}', '{lab}', 'Lab value', '', true, \
                     'manual', 'low')"
        ),
    )
    .await;

    let (status, text) = add_formula(&f, &script_id, "Dissolved_O2 * Lab_value").await;
    assert!(
        (200..300).contains(&status),
        "add the formula ({status}): {text}"
    );
    let output_id = parameter_id(&db, OUTPUT_CODE)
        .await
        .expect("the formula save minted the output parameter");

    let (status, applied) = apply(&f, &script_id, false).await;
    assert_eq!(status, 200, "{applied}");
    assert_eq!(
        slot(&db, &output_id).await.expect("the slot").2,
        "low",
        "read at the instant, the lab value is a visit slot and the set is the chain's"
    );
}

/// Scenario: the calculation reads the oxygen and the temperature the site streams.
///
/// Expected behaviour: the apply is refused naming both. A stream calculation transforms one
/// measured input at the same pulse (Q252), so two stream inputs have no instant to meet at.
#[tokio::test]
#[serial]
async fn a_calculation_reading_two_stream_inputs_is_not_applied_on_the_stream() {
    let f = crate::common::seeded_app().await;
    let db = f.db.clone();
    let script_id = seed_calculation(&db).await;

    let (status, text) = add_formula(&f, &script_id, "Dissolved_O2 * DO_Temperature").await;
    assert!((200..300).contains(&status), "({status}): {text}");
    let output_id = parameter_id(&db, OUTPUT_CODE).await.expect("the output");

    let (status, refused) = apply(&f, &script_id, false).await;
    assert_eq!(status, 400, "{refused}");
    let message = refused.to_string();
    assert!(
        message.contains("Dissolved_O2") && message.contains("DO_Temperature"),
        "{message}"
    );
    assert_eq!(
        slot(&db, &output_id).await,
        None,
        "a refused apply writes no slot"
    );
}

/// Scenario: a calculation on the stream arm at a site gains a second input in a save.
///
/// Expected behaviour: the save is refused naming the site, and the version the stream runs stays
/// the one it was.
#[tokio::test]
#[serial]
async fn a_save_giving_a_stream_calculation_a_second_input_is_refused() {
    let f = crate::common::seeded_app().await;
    let db = f.db.clone();
    let script_id = seed_calculation(&db).await;

    let (status, text) = add_formula(&f, &script_id, "DO_Temperature * 2").await;
    assert!((200..300).contains(&status), "({status}): {text}");
    let (status, applied) = apply(&f, &script_id, false).await;
    assert_eq!(status, 200, "{applied}");
    let output_id = parameter_id(&db, OUTPUT_CODE).await.expect("the output");
    assert_eq!(slot(&db, &output_id).await.expect("the slot").2, "high");

    let (status, refused) = add_formula(&f, &script_id, "DO_Temperature * Dissolved_O2").await;
    assert_eq!(status, 400, "{refused}");
    assert!(
        refused.contains("Dissolved_O2") && refused.contains("DO_Temperature"),
        "{refused}"
    );
    let formula = crate::common::e2e::scalar(
        &db,
        &format!("SELECT formula FROM calculation_formulas WHERE code = '{OUTPUT_CODE}'"),
    )
    .await;
    assert_eq!(
        formula, "DO_Temperature * 2",
        "the refused save wrote nothing"
    );
}
