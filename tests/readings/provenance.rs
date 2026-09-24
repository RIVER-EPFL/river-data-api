//! The provenance resolver: one instant of one series, addressed by stream or by slot, answered
//! with the assembled record (origin, per-replicate corrections, event, computation, state).
//!
//! Run: cargo test --test readings provenance -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use river_db::routes::private::readings::models as decision_models;
use river_db::routes::private::readings::service as decisions;

use crate::common::sensor_lifecycle::create_sensor_without_curve;
use crate::common::{GLOBAL_PARAM_DO_ID, PARAM_S1_DO_ID, SITE1_ID};

const T1: &str = "2025-06-01T08:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

fn slot_uri(time: &str) -> String {
    format!(
        "/api/readings/provenance?site_id={SITE1_ID}&parameter_id={GLOBAL_PARAM_DO_ID}&time={}",
        time.replace('+', "%2B")
    )
}

async fn save_grab(app: &axum::Router, token: &str) {
    let (status, body) = crate::common::post_checked_grab(
        app,
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 12.0, "time": T1 },
            ],
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
#[serial]
async fn the_slot_form_assembles_a_grab_instant() {
    let (_db, app, token) = setup().await;
    save_grab(&app, &token).await;

    let (status, body) = crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["duplicate_slot"], false);
    let records = body["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    let rec = &records[0];
    assert_eq!(rec["origin"]["classification"], "manual");
    assert_eq!(rec["origin"]["source_system"], "grab_sample");
    assert!(
        rec["origin"]["ingested_at"].is_string(),
        "a fresh insert carries its arrival stamp: {rec}"
    );
    assert_eq!(rec["readings"].as_array().unwrap().len(), 2);
    assert_eq!(rec["event"]["source"], "manual");
    assert!(
        rec["computation"]["sample_id"].is_string(),
        "the replicate group's sample is the computation handle: {rec}"
    );

    // The stream form answers identically for the same rows.
    let stream_id = rec["origin"]["stream_id"].as_str().unwrap();
    let (status, by_stream) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/provenance?stream_id={stream_id}&time={T1}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{by_stream}");
    assert_eq!(by_stream["records"][0]["readings"], rec["readings"]);
}

#[tokio::test]
#[serial]
async fn flag_and_withdrawal_state_travel_on_the_facets() {
    let (db, app, token) = setup().await;
    save_grab(&app, &token).await;

    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "UPDATE readings SET is_flagged = TRUE, flag_reason = 'outlier' \
             WHERE site_id = '{SITE1_ID}' AND time = '{T1}' AND replicate_index = 0"
        ),
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "UPDATE readings SET withdrawn_at = NOW(), withdrawn_reason = 'absent from window' \
             WHERE site_id = '{SITE1_ID}' AND time = '{T1}' AND replicate_index = 1"
        ),
    ))
    .await
    .unwrap();

    let (status, body) = crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    assert_eq!(status, 200, "{body}");
    let readings = body["records"][0]["readings"].as_array().unwrap();
    assert_eq!(readings[0]["is_flagged"], true);
    assert_eq!(readings[0]["flag_reason"], "outlier");
    assert!(readings[1]["withdrawn_at"].is_string());
    assert_eq!(readings[1]["withdrawn_reason"], "absent from window");
}

#[tokio::test]
#[serial]
async fn a_standard_curve_reference_names_its_instrument() {
    let (db, app, token) = setup().await;
    save_grab(&app, &token).await;
    let sensor_id = create_sensor_without_curve(&db, "Lab fluorometer").await;
    let (status, curve) = crate::common::post_json_parse_with_token(
        &app,
        "/api/standard_curves",
        &json!({"sensor_id": sensor_id, "name": "Plate 7", "slope": 2.0, "intercept": 1.0}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{curve}");
    let curve_id = curve["id"].as_str().unwrap();
    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "UPDATE readings SET standard_curve_id = '{curve_id}' \
             WHERE site_id = '{SITE1_ID}' AND time = '{T1}' AND replicate_index = 0"
        ),
    ))
    .await
    .unwrap();

    let (status, body) = crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    assert_eq!(status, 200, "{body}");
    let curve_ref = &body["records"][0]["readings"][0]["standard_curve"];
    assert_eq!(curve_ref["id"], curve_id);
    assert_eq!(curve_ref["name"], "Plate 7");
    assert_eq!(curve_ref["sensor_id"], sensor_id.to_string());
}

#[tokio::test]
#[serial]
async fn two_streams_on_one_slot_report_duplicate_slot() {
    let (db, app, token) = setup().await;
    let (sync_token, _service) = crate::common::seed_sync_session_token(&db).await;

    // The first column is one the portal computes, and says so on its descriptor; the second is
    // stored as entered, and is one of the columns the first reads.
    let computed = json!({
        "parameter": {
            "source_calculation": {
                "function": "calcPCO2",
                "inputs": ["lab_co2_co2ppm", "WTW_Temp_degC_1"],
            },
        },
    });
    let entered = json!({ "parameter": { "column_name": "lab_co2_co2ppm" } });
    for (key, metadata) in [("stn:a", computed), ("stn:b", entered)] {
        let (status, stream) = crate::common::post_json_parse_with_token(
            &app,
            "/api/streams/register",
            &json!({
                "source_system": "cnet",
                "source_key": key,
                "measurement_type": "spot",
                "metadata": metadata,
            }),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "{stream}");
        let stream_id = crate::common::e2e::id_of(&stream);
        let (status, body) = crate::common::post_json_with_token(
            &app,
            &format!("/api/streams/{stream_id}/pair"),
            &json!({"site_parameter_id": PARAM_S1_DO_ID}),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/ingest",
            &json!({
                "stream_id": stream_id,
                "readings": [{ "time": T1, "raw_value": 5.0, "replicate_index": 0 }],
            }),
            &sync_token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }

    let (status, body) = crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["duplicate_slot"], true);
    assert_eq!(body["records"].as_array().unwrap().len(), 2);
    for rec in body["records"].as_array().unwrap() {
        assert_eq!(rec["origin"]["classification"], "sync");
        let calculation = &rec["origin"]["portal_calculation"];
        if rec["origin"]["source_key"] == "stn:a" {
            assert_eq!(calculation["function"], "calcPCO2", "{rec}");
            let inputs = calculation["inputs"].as_array().expect("inputs");
            assert_eq!(inputs[0]["column"], "lab_co2_co2ppm");
            assert_eq!(
                inputs[0]["point"]["site_parameter_id"], PARAM_S1_DO_ID,
                "a column the site holds at the instant opens its record: {rec}"
            );
            assert_eq!(inputs[1]["column"], "WTW_Temp_degC_1");
            assert!(
                inputs[1]["point"].is_null(),
                "a column nothing holds opens nothing: {rec}"
            );
        } else {
            assert!(calculation.is_null(), "an entered column names none: {rec}");
        }
    }
}

#[tokio::test]
#[serial]
async fn holds_touching_the_instant_are_listed() {
    let (db, app, token) = setup().await;
    save_grab(&app, &token).await;

    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO replicate_audit_holds \
                 (kind, site_id, parameter_id, group_time, tool, expected, computed, delta, status) \
             VALUES ('stale_output', '{SITE1_ID}', '{GLOBAL_PARAM_DO_ID}', '{T1}', 'chain_b', \
                     '{{\"value\": 55.0}}', '{{\"value\": 47.0}}', '{{}}', 'pending')"
        ),
    ))
    .await
    .unwrap();

    let (status, body) = crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    assert_eq!(status, 200, "{body}");
    let holds = body["records"][0]["holds"].as_array().unwrap();
    assert_eq!(holds.len(), 1, "{body}");
    assert_eq!(holds[0]["kind"], "stale_output");
    assert_eq!(holds[0]["status"], "pending");
    assert_eq!(holds[0]["tool"], "chain_b");
}

#[tokio::test]
#[serial]
async fn missing_instant_and_missing_key_are_refused() {
    let (_db, app, token) = setup().await;

    let (status, _) =
        crate::common::get_json_with_token(&app, &slot_uri("2030-01-01T00:00:00Z"), &token).await;
    assert_eq!(status, 404);

    let (status, _) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/provenance?time={T1}"),
        &token,
    )
    .await;
    assert_eq!(status, 400);
}

/// Scenario: clicking a derived point the chart drew on the continuous line.
///
/// Expected behaviour: the record comes back. The readings query serves `continuous` as everything
/// that is not a grab, which is what puts derived rows on that line in the first place, so the
/// resolver has to read the word the same way or the chart can draw a point it cannot explain.
#[tokio::test]
#[serial]
async fn the_continuous_cadence_resolves_a_derived_reading() {
    let (db, app, token) = setup().await;

    crate::common::seed_data_stream(
        &db,
        "00000000-0000-4000-c000-0000000009e1",
        "test",
        "derived_provenance",
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, time, replicate_index, site_id, parameter_id, raw_value, \
                  measurement_type) \
             VALUES ('00000000-0000-4000-c000-0000000009e1', '{T1}', 0, '{SITE1_ID}', \
                     '{GLOBAL_PARAM_DO_ID}', 7.5, 'derived')"
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("{}&measurement_type=continuous", slot_uri(T1)),
        &token,
    )
    .await;
    assert_eq!(
        status, 200,
        "a derived row is served on the continuous line, so it resolves there: {body}"
    );
    let records = body["records"].as_array().expect("records");
    assert_eq!(records.len(), 1, "{body}");
}

/// Scenario: a derived value over DO at an instant holding both a logger reading and a grab pair.
///
/// Expected behaviour: the record names the input the formula read, the continuous reading, not
/// the grab pair's mean.
#[tokio::test]
#[serial]
async fn a_derived_input_is_the_reading_the_compute_chose() {
    let (db, app, token) = setup().await;
    use crate::common::GLOBAL_PARAM_TURB_ID;
    save_grab(&app, &token).await;

    let logger = "00000000-0000-4000-c000-0000000009f1";
    let output = "00000000-0000-4000-c000-0000000009f2";
    let definition = "00000000-0000-4000-c000-0000000009f3";
    crate::common::seed_data_stream(&db, logger, "test", "do_logger").await;
    crate::common::seed_data_stream(&db, output, "test", "derived_output").await;
    // The formula belongs to a calculation: one owned by none is a shared step (Q156), which the
    // `calculation_formulas_owner_or_step` CHECK refuses outright when it is not marked one.
    let calculation = crate::common::seed_formula_calculation(&db, "do_twice_set").await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO calculation_formulas \
                 (id, code, name, units, formula, output_parameter_id, tool_script_id) \
             VALUES ('{definition}', 'DoTwice', 'DO twice', '', 'do_val * 2', \
                 '{GLOBAL_PARAM_TURB_ID}', '{calculation}')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO derived_parameter_sources (derived_definition_id, parameter_id, variable_name) \
             VALUES ('{definition}', '{GLOBAL_PARAM_DO_ID}', 'do_val')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, time, replicate_index, site_id, parameter_id, raw_value, measurement_type) \
             VALUES ('{logger}', '{T1}', 0, '{SITE1_ID}', '{GLOBAL_PARAM_DO_ID}', 7.0, 'continuous'), \
                    ('{output}', '{T1}', 0, '{SITE1_ID}', '{GLOBAL_PARAM_TURB_ID}', 14.0, 'derived')"
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/readings/provenance?site_id={SITE1_ID}&parameter_id={GLOBAL_PARAM_TURB_ID}&time={T1}"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let input = &body["records"][0]["inputs"][0];
    assert_eq!(input["variable_name"], json!("do_val"), "{body}");
    assert_eq!(input["value"].as_f64(), Some(7.0), "{body}");
    assert_eq!(input["served_as"], json!("reading"), "{body}");
}

/// Scenario: the four write paths an operator can reach land a reading each, and a row is
/// inserted with no origin declared at all.
///
/// Expected behaviour: every reading says where it came from. The paths that know stamp what they
/// are; the row that declares nothing takes the kind its own stream proves, so the column is total
/// and "unknown origin" is a named kind rather than a NULL.
#[tokio::test]
#[serial]
async fn every_reading_says_where_it_came_from() {
    let (db, app, token) = setup().await;
    save_grab(&app, &token).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "time": "2025-06-01T09:00:00Z",
                "raw_value": 4.0,
            }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    crate::common::seed_data_stream(
        &db,
        "00000000-0000-4000-c000-0000000009e2",
        "vaisala",
        "kind-sync",
    )
    .await;
    crate::common::exec(
        &db,
        "INSERT INTO readings (stream_id, time, replicate_index, raw_value) \
         VALUES ('00000000-0000-4000-c000-0000000009e2', '2025-06-01T10:00:00Z', 0, 1.0)",
    )
    .await;

    assert_eq!(
        kind_of(
            &db,
            "SELECT r.provenance_kind AS v FROM readings r JOIN data_streams ds              ON ds.id = r.stream_id WHERE ds.source_system = 'grab_sample' LIMIT 1",
        )
        .await,
        "manual",
        "a hand entry names the person's path"
    );
    assert_eq!(
        kind_of(
            &db,
            "SELECT r.provenance_kind AS v FROM readings r JOIN data_streams ds              ON ds.id = r.stream_id WHERE ds.source_system = 'api' LIMIT 1",
        )
        .await,
        "batch"
    );
    assert_eq!(
        kind_of(
            &db,
            "SELECT provenance_kind AS v FROM readings              WHERE stream_id = '00000000-0000-4000-c000-0000000009e2'",
        )
        .await,
        "sync",
        "a writer that declares nothing takes what its stream proves"
    );

    let untotal: i64 = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*) AS v FROM readings WHERE provenance_kind IS NULL".to_string(),
        ))
        .await
        .expect("query")
        .expect("a row")
        .try_get("", "v")
        .expect("count");
    assert_eq!(untotal, 0, "no reading is stored without an origin");
}

async fn kind_of(db: &DatabaseConnection, sql: &str) -> String {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .expect("query")
    .expect("a reading")
    .try_get::<String>("", "v")
    .expect("provenance_kind")
}

/// Scenario: `ingested_at` is the row's first arrival and nothing moves it (Q86), so the panel
/// needs a second instant to answer when the number it displays appeared.
///
/// Expected behaviour: `value_arrived_at` is the latest live value correction's `at` where there
/// is one, the first arrival otherwise, and it returns to the first arrival when the correction is
/// rolled back.
#[tokio::test]
#[serial]
async fn the_arrival_of_the_current_value_is_the_correction_that_wrote_it() {
    let (db, app, token) = setup().await;
    save_grab(&app, &token).await;

    let (_, body) = crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    let rec = &body["records"][0];
    let first = rec["origin"]["ingested_at"].as_str().unwrap().to_string();
    assert_eq!(
        rec["origin"]["value_arrived_at"].as_str().unwrap(),
        first,
        "with no correction the current value arrived when the row did: {rec}"
    );

    let stream_id: uuid::Uuid = rec["origin"]["stream_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let correction = decisions::Decision {
        key: decisions::DecisionKey {
            stream_id,
            time: T1.parse().unwrap(),
            replicate_index: Some(0),
        },
        kind: decision_models::Kind::ValueCorrection,
        new: json!({ "raw_value": 11.0 }),
        actor: "tester".to_string(),
        reason: Some("re-read".to_string()),
        origin: decision_models::Origin::Manual,
        set_id: None,
    };
    let decision_id = river_db::common::bulk_write::guarded(&db, async |txn| {
        decisions::record(txn, &correction).await
    })
    .await
    .unwrap();

    let (_, body) = crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    let rec = &body["records"][0];
    let corrected = rec["origin"]["value_arrived_at"].as_str().unwrap();
    assert_ne!(
        corrected, first,
        "the corrected value arrived when the correction did: {rec}"
    );
    assert_eq!(
        rec["origin"]["ingested_at"].as_str().unwrap(),
        first,
        "the first arrival is untouched: {rec}"
    );
    let corrected_row = rec["readings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["replicate_index"] == 0)
        .unwrap();
    assert_eq!(
        corrected_row["value_arrived_at"].as_str().unwrap(),
        corrected
    );
    let other = rec["readings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["replicate_index"] == 1)
        .unwrap();
    assert_eq!(
        other["value_arrived_at"], other["ingested_at"],
        "a replicate nothing corrected still reports its own arrival: {rec}"
    );

    decisions::rollback(&db, decision_id, "tester", Some("undo"))
        .await
        .unwrap();
    let (_, body) = crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    assert_eq!(
        body["records"][0]["origin"]["value_arrived_at"]
            .as_str()
            .unwrap(),
        first,
        "a rolled-back correction is not the arrival of anything: {body}"
    );
}

const CHAIN_AT: &str = "2025-07-02T09:00:00Z";
const CHAIN_GROUP_ID: &str = "00000000-0000-4000-c000-000000000202";

/// Puts a chain parameter in the group and declares it at the site. `mint` also creates the
/// catalog row; a calculation's output already has one.
async fn declare_chain_parameter(
    db: &DatabaseConnection,
    id: &str,
    code: &str,
    ordinal: i32,
    mint: bool,
) {
    if mint {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO parameters (id, code, name, default_units, category) \
                 VALUES ('{id}', '{code}', '{code}', 'ppb', 'measurement')"
            ),
        )
        .await;
    }
    for sql in [
        format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{CHAIN_GROUP_ID}', '{id}', {ordinal})"
        ),
        format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, is_active) \
             VALUES (gen_random_uuid(), '{SITE1_ID}', '{id}', '{code}', 'lab', true)"
        ),
    ] {
        crate::common::exec(db, &sql).await;
    }
}

async fn minted_chain_output(db: &DatabaseConnection, code: &str) -> String {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT id FROM parameters WHERE lower(code) = lower('{code}')"),
    ))
    .await
    .expect("the catalog reads")
    .expect("the calculation minted its output")
    .try_get::<uuid::Uuid>("", "id")
    .expect("id")
    .to_string()
}

/// A two-stage, three-replicate calculation over a measured family: `S1 = Peak * 2` per
/// replicate, `S2 = S1 + 1` over the family's mean. Returns the three parameter ids.
async fn seed_chain(
    db: &DatabaseConnection,
    app: &axum::Router,
    token: &str,
) -> (String, String, String) {
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = 'chain'".to_string(),
        "DELETE FROM tool_scripts WHERE name = 'chain'".to_string(),
        format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal) \
             VALUES ('{CHAIN_GROUP_ID}', 'chain', 'Chain', 1)"
        ),
    ] {
        crate::common::exec(db, &sql).await;
    }
    let peak_id = uuid::Uuid::new_v4().to_string();
    declare_chain_parameter(db, &peak_id, "Peak", 1, true).await;
    crate::common::exec(
        db,
        "INSERT INTO tool_scripts (name, label, engine, created_by) \
             VALUES ('chain', 'Chain', 'formula', 'test')",
    )
    .await;
    let script_id = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT id FROM tool_scripts WHERE name = 'chain'".to_string(),
        ))
        .await
        .expect("query")
        .expect("the calculation")
        .try_get::<uuid::Uuid>("", "id")
        .expect("id")
        .to_string();
    let (status, text) = crate::common::save_formula_set(
        app,
        token,
        &script_id,
        json!([
            { "code": "S1", "name": "S1", "units": "ppb", "formula": "Peak * 2",
              "ordinal": 1, "per_replicate": "Peak" },
            { "code": "S2", "name": "S2", "units": "ppb", "formula": "S1 + 1", "ordinal": 2 }
        ]),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the formulas ({status}): {text}"
    );
    // The calculation minted the two outputs; the site declares them under the ids it chose.
    let mut minted = Vec::new();
    for (code, ordinal) in [("S1", 2), ("S2", 3)] {
        let id = minted_chain_output(db, code).await;
        declare_chain_parameter(db, &id, code, ordinal, false).await;
        minted.push(id);
    }
    (peak_id, minted[0].clone(), minted[1].clone())
}

async fn save_at_chain_instant(
    app: &axum::Router,
    token: &str,
    run_id: Option<&serde_json::Value>,
    readings: serde_json::Value,
) {
    let mut body = json!({ "site_id": SITE1_ID, "readings": readings });
    if let Some(run_id) = run_id {
        body["tool_run_id"] = run_id.clone();
    }
    let (status, text) = crate::common::post_checked_grab(app, &body, token).await;
    assert_eq!(status, 200, "save ({status}): {text}");
}

async fn calculate_chain(app: &axum::Router, token: &str) -> serde_json::Value {
    let (status, text) = crate::common::post_json_with_token(
        app,
        "/api/tools/chain/calculate",
        &json!({ "Peak": [1.0, 2.0, 3.0], "site_id": SITE1_ID, "collected_at": CHAIN_AT }),
        token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {text}");
    serde_json::from_str(&text).expect("JSON")
}

async fn record_of(app: &axum::Router, token: &str, parameter_id: &str) -> serde_json::Value {
    let (status, body) = crate::common::get_json_with_token(
        app,
        &format!(
            "/api/readings/provenance?site_id={SITE1_ID}&parameter_id={parameter_id}&time={CHAIN_AT}"
        ),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["records"][0].clone()
}

fn values(
    section: &serde_json::Value,
    code_field: &str,
    code: &str,
) -> Vec<(Option<i64>, Option<f64>)> {
    section
        .as_array()
        .expect("a section")
        .iter()
        .filter(|entry| entry[code_field] == json!(code))
        .map(|entry| (entry["replicate_index"].as_i64(), entry["value"].as_f64()))
        .collect()
}

/// Scenario: a two-stage chain over two letters, computed and saved at one visit.
///
/// Expected behaviour: standing on any link, the record names the values one hop above it and
/// the formulas one hop below it, each with the key that resolves its own record, so the chain
/// from raw replicate to published statistic is walkable in both directions.
#[tokio::test]
#[serial]
async fn a_per_replicate_chain_is_walkable_in_both_directions() {
    let (db, app, token) = setup().await;
    let (peak_id, s1_id, s2_id) = seed_chain(&db, &app, &token).await;

    save_at_chain_instant(
        &app,
        &token,
        None,
        json!([
            { "parameter_id": peak_id, "value": 1.0, "time": CHAIN_AT, "replicate_index": 0 },
            { "parameter_id": peak_id, "value": 2.0, "time": CHAIN_AT, "replicate_index": 1 },
            { "parameter_id": peak_id, "value": 3.0, "time": CHAIN_AT, "replicate_index": 2 },
        ]),
    )
    .await;
    let run = calculate_chain(&app, &token).await;
    save_at_chain_instant(
        &app,
        &token,
        Some(&run["run_id"]),
        json!([
            { "parameter_id": s1_id, "value": 2.0, "time": CHAIN_AT, "replicate_index": 0, "output": "S1" },
            { "parameter_id": s1_id, "value": 4.0, "time": CHAIN_AT, "replicate_index": 1, "output": "S1" },
            { "parameter_id": s1_id, "value": 6.0, "time": CHAIN_AT, "replicate_index": 2, "output": "S1" },
        ]),
    )
    .await;
    let run = calculate_chain(&app, &token).await;
    assert_eq!(run["results"]["S2"].as_f64(), Some(5.0), "{run}");
    save_at_chain_instant(
        &app,
        &token,
        Some(&run["run_id"]),
        json!([
            { "parameter_id": s2_id, "value": 5.0, "time": CHAIN_AT, "replicate_index": 0, "output": "S2" },
        ]),
    )
    .await;

    // The raw family: nothing above it, S1 below it at each index.
    let peak = record_of(&app, &token, &peak_id).await;
    assert!(
        peak.get("inputs").is_none(),
        "a measured value has no inputs: {peak}"
    );
    assert_eq!(
        values(&peak["consumers"], "formula_code", "S1"),
        vec![
            (Some(0), Some(2.0)),
            (Some(1), Some(4.0)),
            (Some(2), Some(6.0))
        ],
        "{peak}"
    );
    assert_eq!(peak["consumers"][0]["output_parameter_id"], json!(s1_id));
    assert_eq!(peak["consumers"][0]["calculation"], json!("chain"));

    // The per-replicate stage: Peak above it index by index, S2 below it over the mean.
    let s1 = record_of(&app, &token, &s1_id).await;
    assert_eq!(
        values(&s1["inputs"], "variable_name", "Peak"),
        vec![
            (Some(0), Some(1.0)),
            (Some(1), Some(2.0)),
            (Some(2), Some(3.0))
        ],
        "{s1}"
    );
    assert_eq!(s1["inputs"][0]["parameter_id"], json!(peak_id));
    assert_eq!(s1["inputs"][0]["served_as"], json!("replicate"));
    assert_eq!(
        values(&s1["consumers"], "formula_code", "S2"),
        vec![(None, Some(5.0))],
        "{s1}"
    );

    // The published statistic: S1's mean above it, nothing below it.
    let s2 = record_of(&app, &token, &s2_id).await;
    assert_eq!(
        values(&s2["inputs"], "variable_name", "S1"),
        vec![(None, Some(4.0))],
        "the mean of 2, 4 and 6: {s2}"
    );
    assert_eq!(s2["inputs"][0]["served_as"], json!("mean"));
    assert_eq!(s2["inputs"][0]["parameter_id"], json!(s1_id));
    assert!(s2.get("consumers").is_none(), "{s2}");

    // A decommissioned calculation is no longer a consumer; what it produced keeps its inputs.
    crate::common::exec(
        &db,
        "UPDATE tool_scripts SET decommissioned_at = now(), decommissioned_by = 'test', \
         decommission_reason = 'retired' WHERE name = 'chain'",
    )
    .await;
    let peak = record_of(&app, &token, &peak_id).await;
    assert!(peak.get("consumers").is_none(), "{peak}");
    let s1 = record_of(&app, &token, &s1_id).await;
    assert_eq!(s1["inputs"].as_array().map(Vec::len), Some(3), "{s1}");
}

/// Scenario: a derived value is computed from a stored reading.
///
/// Expected behaviour: the record names what the computation consumed, each input carrying the
/// key of the reading it was read from and the slot its own point record is opened by (Q215,
/// M276). What the marks read as when a source moves is `consumed_history.rs`, which walks each
/// path that can move one.
#[tokio::test]
#[serial]
async fn a_consumed_input_opens_the_reading_it_was_read_from() {
    let (db, app, token) = setup().await;

    let code = format!("consumed_{}", uuid::Uuid::new_v4().simple());
    let calculation = crate::common::seed_formula_calculation(&db, &format!("{code}_set")).await;
    let (status, def) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": code,
            "name": "Consumed input fixture",
            "units": "mg/L",
            "formula": "Dissolved_O2 * 2",
            "tool_script_id": calculation,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {def}");
    crate::common::commit_calculation(&db, calculation).await;
    let output = def["output_parameter_id"].as_str().expect("output");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &json!({
            "site_id": SITE1_ID,
            "parameter_id": output,
            "name": code,
            "sensor_type": "derived",
            "entry_mode": "tool",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "assign ({status}): {body}");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "time": T1,
                "raw_value": 10.0,
            }]
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "ingest ({status}): {body}");

    let uri =
        format!("/api/readings/provenance?site_id={SITE1_ID}&parameter_id={output}&time={T1}");
    let consumed = wait_for_consumed(&app, &token, &uri).await;
    assert_eq!(
        consumed.len(),
        2,
        "the input and the formula step: {consumed:?}"
    );
    let input = entry(&consumed, "Dissolved_O2");
    assert_eq!(input["state"], json!("unchanged"), "{input}");
    assert_eq!(input["value"].as_f64(), Some(10.0), "{input}");
    let member = &input["members"][0];
    assert_eq!(member["value"].as_f64(), Some(10.0), "{member}");
    assert_eq!(member["current_value"].as_f64(), Some(10.0), "{member}");
    assert_eq!(member["state"], json!("unchanged"), "{member}");
    assert_eq!(
        member["point"]["site_parameter_id"],
        json!(PARAM_S1_DO_ID),
        "the member opens the slot's own point record: {member}"
    );
    assert_eq!(
        member["point"]["measurement_type"],
        json!("continuous"),
        "{member}"
    );
    assert_eq!(entry(&consumed, &code)["state"], json!("unchanged"));
}

/// The record's consumed set, polled until the compute that captures it lands.
async fn wait_for_consumed(app: &axum::Router, token: &str, uri: &str) -> Vec<serde_json::Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (status, body) = crate::common::get_json_with_token(app, uri, token).await;
        let set = body["records"][0]["consumed"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !set.is_empty() || std::time::Instant::now() >= deadline {
            assert_eq!(status, 200, "{body}");
            return set;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

fn entry<'a>(consumed: &'a [serde_json::Value], variable: &str) -> &'a serde_json::Value {
    consumed
        .iter()
        .find(|c| c["variable"] == variable)
        .unwrap_or_else(|| panic!("{variable} was consumed: {consumed:?}"))
}
