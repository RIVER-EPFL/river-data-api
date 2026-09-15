//! Scenario: the two-stage shape Q95 decided, joined through storage rather than inside one run.
//!
//! A calculation's first formula evaluates once per replicate index and its repeats are stored
//! under their own catalog parameter; the `samples` trigger derives their mean; the second formula
//! reads that mean back as an event input and produces the calculation's answer.
//!
//! Each half has a test of its own. What this pins is the join: a repeat landing at the wrong
//! index, or a `samples` row that never materialises, is a wrong second-stage number with nothing
//! else failing.

use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;

const CALCULATION: &str = "two_stage";
const GROUP_ID: &str = "00000000-0000-4000-c000-000000000201";
const AT: &str = "2025-07-02T09:00:00Z";

/// The group, its members and the calculation bound to it, written directly: `tool_scripts` is
/// authored through Administrator-only routes and what this suite is about is what happens once a
/// two-stage calculation exists.
async fn seed_two_stage(db: &sea_orm::DatabaseConnection) -> String {
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
             VALUES ('{GROUP_ID}', 'two_stage', 'Two stage', 1)"
        ),
    )
    .await;

    // Only the entry the family is measured into is declared here. S1 and S2 are minted by the
    // formulas that publish them (Q191), and placed in the group afterwards (Q189).
    let peak_id = uuid::Uuid::new_v4().to_string();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{peak_id}', 'Peak', 'Peak', 'ppb', 'measurement')"
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
             VALUES ('{CALCULATION}', 'Two stage', 'formula', 'test')"
        ),
    )
    .await;
    peak_id
}

/// The parameter a saved formula minted, put in the group and given a slot at site 1, which is
/// what a person does on the group page once the calculation publishes it.
async fn place_output(db: &sea_orm::DatabaseConnection, code: &str, ordinal: i32) -> String {
    let id = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT id FROM parameters WHERE lower(code) = lower('{code}')"),
        ))
        .await
        .expect("query")
        .expect("the formula minted its output")
        .try_get::<uuid::Uuid>("", "id")
        .expect("id")
        .to_string();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{GROUP_ID}', '{id}', {ordinal})"
        ),
    )
    .await;
    configure_slot(db, &id, code).await;
    id
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

/// A slot at site 1 for a catalog parameter, which a save's readings need.
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

/// The calculation's formulas, saved as the set they are: the save is what mints the version the
/// run reads.
async fn save_set(
    app: &axum::Router,
    token: &str,
    script_id: &str,
    formulas: serde_json::Value,
) -> (u16, String) {
    crate::common::save_formula_set(app, token, script_id, formulas).await
}

#[tokio::test]
#[serial]
async fn a_stage_one_family_is_stored_per_index_and_stage_two_reads_its_mean() {
    let f = crate::common::seeded_app().await;
    let (db, app, token) = (f.db, f.app, f.token);
    let peak_id = seed_two_stage(&db).await;
    let script_id = calculation_id(&db).await;
    configure_slot(&db, &peak_id, "Peak").await;

    let (status, text) = save_set(
        &app,
        &token,
        &script_id,
        json!([
            { "code": "S1", "name": "S1", "units": "ppb",
              "formula": "Peak * 2", "ordinal": 1, "per_replicate": "Peak" },
            { "code": "S2", "name": "S2", "units": "ppb", "formula": "S1 + 1", "ordinal": 2 }
        ]),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the two stages ({status}): {text}"
    );
    let s1_id = place_output(&db, "S1", 2).await;
    let _s2_id = place_output(&db, "S2", 3).await;

    // Stage one, over a three-member family.
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &json!({
            "Peak": [1.0, 2.0, 3.0],
            "site_id": crate::common::SITE1_ID,
            "collected_at": AT,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {text}");
    let run: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        run["results"]["S1"],
        json!([2.0, 4.0, 6.0]),
        "one value per index: {text}"
    );

    // The repeats are saved under their own parameter, one reading per index.
    let readings: Vec<serde_json::Value> = [2.0, 4.0, 6.0]
        .iter()
        .enumerate()
        .map(|(index, value)| {
            json!({
                "parameter_id": s1_id, "value": value, "time": AT,
                "replicate_index": index, "output": "S1",
            })
        })
        .collect();
    let (status, saved) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": crate::common::SITE1_ID,
            "tool_run_id": run["run_id"],
            "readings": readings,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "save stage one ({status}): {saved}");

    let stored = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT count(*)::bigint AS n, \
                        COALESCE(string_agg(r.replicate_index::text, ',' ORDER BY \
                                            r.replicate_index), '') AS indexes \
                   FROM readings r \
                  WHERE r.site_id = '{site}' AND r.parameter_id = '{s1_id}' AND r.time = '{AT}'",
                site = crate::common::SITE1_ID,
            ),
        ))
        .await
        .expect("query")
        .expect("a row");
    assert_eq!(stored.try_get::<i64>("", "n").expect("n"), 3);
    assert_eq!(
        stored.try_get::<String>("", "indexes").expect("indexes"),
        "0,1,2",
        "each repeat keeps its own index"
    );

    // The trigger derives the family's statistics; no formula computed them.
    let sample = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT s.n, s.mean FROM samples s \
                  WHERE s.site_id = '{site}' AND s.parameter_id = '{s1_id}' \
                    AND s.collected_at = '{AT}'",
                site = crate::common::SITE1_ID,
            ),
        ))
        .await
        .expect("query")
        .expect("the trigger materialised the sample");
    assert_eq!(sample.try_get::<i32>("", "n").expect("n"), 3);
    assert_eq!(
        sample.try_get::<Option<f64>>("", "mean").expect("mean"),
        Some(4.0),
        "the mean of 2, 4 and 6"
    );

    // Stage two, on the pass after the repeats landed: it reads the stored mean, not a repeat.
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &json!({
            "Peak": [1.0, 2.0, 3.0],
            "site_id": crate::common::SITE1_ID,
            "collected_at": AT,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "second pass ({status}): {text}");
    let run: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        run["results"]["S2"].as_f64(),
        Some(5.0),
        "4 + 1, from the mean rather than from one repeat: {text}"
    );
}

/// A repeat that was not measured leaves its index empty and takes no reading, and the statistics
/// are over what was measured.
#[tokio::test]
#[serial]
async fn a_gap_in_the_family_stays_a_gap() {
    let f = crate::common::seeded_app().await;
    let (db, app, token) = (f.db, f.app, f.token);
    let peak_id = seed_two_stage(&db).await;
    let script_id = calculation_id(&db).await;
    configure_slot(&db, &peak_id, "Peak").await;
    let (status, text) = save_set(
        &app,
        &token,
        &script_id,
        json!([{
            "code": "S1", "name": "S1", "units": "ppb",
            "formula": "Peak * 2", "ordinal": 1, "per_replicate": "Peak",
        }]),
    )
    .await;
    assert!((200..300).contains(&status), "stage one ({status}): {text}");
    let s1_id = place_output(&db, "S1", 2).await;

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &json!({
            "Peak": [1.0, null, 3.0],
            "site_id": crate::common::SITE1_ID,
            "collected_at": AT,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {text}");
    let run: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        run["results"]["S1"],
        json!([2.0, null, 6.0]),
        "the unmeasured repeat is still at index 1: {text}"
    );

    // Only the measured repeats are saved, at the indexes they were measured at.
    let readings = json!([
        { "parameter_id": s1_id, "value": 2.0, "time": AT, "replicate_index": 0, "output": "S1" },
        { "parameter_id": s1_id, "value": 6.0, "time": AT, "replicate_index": 2, "output": "S1" },
    ]);
    let (status, saved) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": crate::common::SITE1_ID,
            "tool_run_id": run["run_id"],
            "readings": readings,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "save ({status}): {saved}");

    let indexes = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COALESCE(string_agg(r.replicate_index::text, ',' ORDER BY \
                                            r.replicate_index), '') AS indexes \
                   FROM readings r \
                  WHERE r.site_id = '{site}' AND r.parameter_id = '{s1_id}' AND r.time = '{AT}'",
                site = crate::common::SITE1_ID,
            ),
        ))
        .await
        .expect("query")
        .expect("a row")
        .try_get::<String>("", "indexes")
        .expect("indexes");
    assert_eq!(
        indexes, "0,2",
        "the gap keeps index 1 rather than closing up onto it"
    );
}
