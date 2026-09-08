//! The provenance resolver: one instant of one series, addressed by stream or by slot, answered
//! with the assembled record (origin, per-replicate corrections, event, computation, state).
//!
//! Run: cargo test --test readings provenance -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use river_db::routes::private::readings::decisions;

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
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
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

    for key in ["stn:a", "stn:b"] {
        let (status, stream) = crate::common::post_json_parse_with_token(
            &app,
            "/api/streams/register",
            &json!({"source_system": "cnet", "source_key": key, "measurement_type": "spot"}),
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

    let stream_id: uuid::Uuid = rec["origin"]["stream_id"].as_str().unwrap().parse().unwrap();
    let correction = decisions::Decision {
        key: decisions::DecisionKey {
            stream_id,
            time: T1.parse().unwrap(),
            replicate_index: Some(0),
        },
        kind: decisions::Kind::ValueCorrection,
        new: json!({ "raw_value": 11.0 }),
        actor: "tester".to_string(),
        reason: Some("re-read".to_string()),
        origin: decisions::Origin::Manual,
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
    assert_eq!(corrected_row["value_arrived_at"].as_str().unwrap(), corrected);
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
