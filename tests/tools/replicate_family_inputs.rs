//! Scenario: a per-replicate calculation at a visit whose replicates are already stored, run by
//! the chain rather than from the tool form.
//!
//! Expected behaviour: the run reads the family out of the visit, one value per replicate index,
//! and publishes one output reading per index. The interactive path sends the list in the request
//! body, so the chain is the only caller that has to read it back.
//!
//! Run: cargo test --test tools replicate_family_inputs -- --test-threads=1

use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;

const CALCULATION: &str = "family_reader";
const GROUP_ID: &str = "00000000-0000-4000-c000-000000000221";
const AT: &str = "2025-08-04T07:30:00Z";
const REPLICATES: [f64; 3] = [420.0, 425.0, 418.0];

/// The group, the family's own catalog parameter and the calculation bound to it.
async fn seed_calculation(db: &sea_orm::DatabaseConnection) -> String {
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name = '{CALCULATION}'"),
        format!("DELETE FROM tool_scripts WHERE name = '{CALCULATION}'"),
    ] {
        crate::common::exec(db, &sql).await;
    }
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal) \
             VALUES ('{GROUP_ID}', 'headspace', 'Headspace', 1)"
        ),
    )
    .await;
    let peak_id = uuid::Uuid::new_v4().to_string();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{peak_id}', 'hs_co2_ppm', 'Headspace CO2', 'ppm', 'measurement')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{GROUP_ID}', '{peak_id}', 1)"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO tool_scripts (name, label, engine, created_by) \
             VALUES ('{CALCULATION}', 'Family reader', 'formula', 'test')"
        ),
    )
    .await;
    configure_slot(db, &peak_id, "hs_co2_ppm").await;
    peak_id
}

async fn calculation_id(db: &sea_orm::DatabaseConnection) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT id FROM tool_scripts WHERE name = '{CALCULATION}'"),
    ))
    .await
    .expect("query")
    .expect("the calculation")
    .try_get::<uuid::Uuid>("", "id")
    .expect("id")
    .to_string()
}

async fn minted_output(db: &sea_orm::DatabaseConnection, code: &str) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT id FROM parameters WHERE lower(code) = lower('{code}')"),
    ))
    .await
    .expect("query")
    .expect("the formula minted its output")
    .try_get::<uuid::Uuid>("", "id")
    .expect("id")
    .to_string()
}

async fn configure_slot(db: &sea_orm::DatabaseConnection, parameter_id: &str, name: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, is_active) \
             VALUES (gen_random_uuid(), '{site}', '{parameter_id}', '{name}', 'lab', true)",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;
}

/// The visit the family was measured at, and the event the chain runs over.
async fn seed_visit(db: &sea_orm::DatabaseConnection, parameter_id: &str) -> uuid::Uuid {
    let event_id = uuid::Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO collection_events (id, site_id, collected_at, source) \
             VALUES ('{event_id}', '{site}', '{AT}', 'manual')",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;
    let stream_id = uuid::Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream_id}', 'grab_sample', '{stream_id}', true)"
        ),
    )
    .await;
    for (index, value) in REPLICATES.iter().enumerate() {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                     raw_value, measurement_type, collection_event_id) \
                 VALUES ('{stream_id}', '{site}', '{parameter_id}', '{AT}', {index}, {value}, \
                     'spot', '{event_id}')",
                site = crate::common::SITE1_ID,
            ),
        )
        .await;
    }
    event_id
}

/// What the output slot holds at the visit, by replicate index.
async fn published(db: &sea_orm::DatabaseConnection, parameter_id: &str) -> Vec<(i16, f64)> {
    db.query_all_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT replicate_index, COALESCE(calibrated_value, raw_value) AS value \
               FROM readings \
              WHERE site_id = '{site}' AND parameter_id = '{parameter_id}' AND time = '{AT}' \
              ORDER BY replicate_index",
            site = crate::common::SITE1_ID,
        ),
    ))
    .await
    .expect("query")
    .into_iter()
    .map(|r| {
        (
            r.try_get::<i16>("", "replicate_index").expect("index"),
            r.try_get::<Option<f64>>("", "value")
                .expect("value")
                .expect("a number"),
        )
    })
    .collect()
}

/// The calculation's formula, saved as the set it is: the save is what mints the version the run
/// reads.
async fn add_formula(
    app: &axum::Router,
    token: &str,
    script_id: &str,
    body: serde_json::Value,
) -> (u16, String) {
    crate::common::save_formula_set(app, token, script_id, json!([body])).await
}

#[tokio::test]
#[serial]
async fn the_chain_reads_the_stored_family_and_publishes_one_output_per_index() {
    let f = crate::common::seeded_app().await;
    let (db, app, token) = (f.db, f.app, f.token);
    let peak_id = seed_calculation(&db).await;
    let script_id = calculation_id(&db).await;

    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        json!({
            "code": "co2_dry_ppm", "name": "co2_dry_ppm", "units": "ppm",
            "formula": "hs_co2_ppm * 2", "ordinal": 1, "per_replicate": "hs_co2_ppm",
        }),
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");
    let output_id = minted_output(&db, "co2_dry_ppm").await;
    configure_slot(&db, &output_id, "co2_dry_ppm").await;

    let event_id = seed_visit(&db, &peak_id).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the recompute runs");
    assert_eq!(
        outcome.skipped.len(),
        0,
        "the family is an input the visit holds: {:?}",
        outcome.skipped
    );
    assert_eq!(outcome.tools_run, 1, "the calculation ran");

    assert_eq!(
        published(&db, &output_id).await,
        vec![(0, 840.0), (1, 850.0), (2, 836.0)],
        "one output per replicate, each computed from the value at its own index"
    );
}

/// A repeat nobody measured is a gap in the family, not a shift: the indexes after it keep theirs.
#[tokio::test]
#[serial]
async fn a_missing_index_leaves_a_gap_rather_than_shifting_the_family() {
    let f = crate::common::seeded_app().await;
    let (db, app, token) = (f.db, f.app, f.token);
    let peak_id = seed_calculation(&db).await;
    let script_id = calculation_id(&db).await;

    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        json!({
            "code": "co2_dry_ppm", "name": "co2_dry_ppm", "units": "ppm",
            "formula": "hs_co2_ppm * 2", "ordinal": 1, "per_replicate": "hs_co2_ppm",
        }),
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");
    let output_id = minted_output(&db, "co2_dry_ppm").await;
    configure_slot(&db, &output_id, "co2_dry_ppm").await;

    let event_id = seed_visit(&db, &peak_id).await;
    crate::common::exec(
        &db,
        &format!(
            "DELETE FROM readings WHERE site_id = '{site}' AND parameter_id = '{peak_id}' \
               AND time = '{AT}' AND replicate_index = 1",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
        .await
        .expect("the recompute runs");

    assert_eq!(
        published(&db, &output_id).await,
        vec![(0, 840.0), (2, 836.0)],
        "index 1 is empty and index 2 is still the third repeat"
    );
}
