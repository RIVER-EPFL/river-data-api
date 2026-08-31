//! The sd estimator as a declared parameter specification, end to end.
//!
//! Scenario: a source ships replicate values plus its own precomputed sd, and computed that sd
//! with the population divisor (n) where this system recomputes with the sample one (n-1). The
//! sources used both conventions over the years, row by row, so nothing can infer which one a
//! slot publishes.
//!
//! Expected behaviour: the disagreement is held, the hold names the population signature, and it
//! **cannot be cleared by acknowledgement** while the slot has declared no estimator. Declaring
//! one resolves it, recomputes the slot's samples under that divisor and stops the comparison
//! disagreeing; reopen puts every part of that back.
//!
//! Run: cargo test --test e2e sd_estimator_declaration -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-07-01T08:00:00Z";
const T2: &str = "2025-07-01T09:00:00Z";
const T3: &str = "2025-07-01T10:00:00Z";

/// 10, 12, 14: mean 12, sample sd 2, population sd 1.632993161855452.
const VALUES: [f64; 3] = [10.0, 12.0, 14.0];
const SAMPLE_SD: f64 = 2.0;
const POPULATION_SD: f64 = 1.632_993_161_855_452;

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
    sync_token: String,
    stream: String,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    // A replicate family declaring a precomputed sd column and NO estimator: the honest state for
    // a portal that used both divisors.
    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": "sdsrc",
            "source_key": "STN:TEMP_avg:reps",
            "measurement_type": "spot",
            "replicates": {
                "source_columns": ["temp_rep_1", "temp_rep_2", "temp_rep_3"],
                "portal_mean_column": "temp_avg",
                "portal_sd_column": "temp_sd",
            },
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "register: {stream}");
    let stream = crate::common::e2e::id_of(&stream);

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");

    Fixture {
        db,
        app,
        token,
        sync_token,
        stream,
    }
}

fn group(time: &str) -> Vec<serde_json::Value> {
    VALUES
        .iter()
        .enumerate()
        .map(|(i, v)| json!({"time": time, "raw_value": v, "replicate_index": i}))
        .collect()
}

/// Ingest the group with the source claiming the population sd it computed.
async fn ingest_population_claim(fx: &Fixture, time: &str) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": fx.stream,
            "readings": group(time),
            "audit": [{"time": time, "expected_mean": 12.0, "expected_sd": POPULATION_SD,
                       "expected_n": 3}],
        }),
        &fx.sync_token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    body
}

async fn holds(fx: &Fixture, extra: &str) -> serde_json::Value {
    let (status, body) = crate::common::get_with_token(
        &fx.app,
        &format!(
            "/api/sync/replicate_audit_holds?stream_id={}{extra}",
            fx.stream
        ),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "list holds ({status}): {body}");
    serde_json::from_str(&body).unwrap()
}

async fn hold_at(fx: &Fixture, time: &str) -> serde_json::Value {
    let listed = holds(fx, "&page_size=100").await;
    listed["holds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["group_time"].as_str().unwrap_or_default().starts_with(&time[..19]))
        .unwrap_or_else(|| panic!("no hold at {time}: {listed}"))
        .clone()
}

async fn resolve(
    fx: &Fixture,
    hold_id: &str,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        &fx.app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/resolve"),
        body,
        &fx.token,
    )
    .await
}

/// (stdev, sd_estimator, sd_estimator_source) for one instant's sample.
async fn sample_row(fx: &Fixture, time: &str) -> (Option<f64>, String, String) {
    let row = fx
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT stdev, sd_estimator, sd_estimator_source FROM samples \
                 WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{time}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no sample at {time}"));
    (
        row.try_get("", "stdev").unwrap(),
        row.try_get("", "sd_estimator").unwrap(),
        row.try_get("", "sd_estimator_source").unwrap(),
    )
}

async fn slot_declaration(fx: &Fixture) -> Option<String> {
    fx.db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT sd_estimator FROM site_parameters WHERE id = '{}'",
                crate::common::PARAM_S1_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<Option<String>>("", "sd_estimator")
        .unwrap()
}

/// The audit annotations a hold's decision minted, as (text, category, start_time).
async fn audit_annotations(fx: &Fixture, hold_id: &str) -> Vec<(String, String, String)> {
    fx.db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT text, category, start_time::text AS start_time FROM annotations \
                 WHERE audit_hold_id = '{hold_id}'"
            ),
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.try_get::<String>("", "text").unwrap(),
                row.try_get::<String>("", "category").unwrap(),
                row.try_get::<String>("", "start_time").unwrap(),
            )
        })
        .collect()
}

async fn hold_status(db: &DatabaseConnection, hold_id: &str) -> String {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT status FROM replicate_audit_holds WHERE id = '{hold_id}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<String>("", "status")
    .unwrap()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[tokio::test]
#[serial]
async fn an_undeclared_estimator_blocks_acceptance_until_it_is_declared() {
    let fx = setup().await;

    // --- The disagreement is admitted, held, and named ---
    ingest_population_claim(&fx, T1).await;
    let readings: i64 = crate::common::e2e::count(
        &fx.db,
        &format!(
            "SELECT COUNT(*) FROM readings WHERE stream_id = '{}' AND time = '{T1}'",
            fx.stream
        ),
    )
    .await;
    assert_eq!(readings, 3, "the audit always admits: every replicate stored");

    let hold = hold_at(&fx, T1).await;
    assert_eq!(hold["classification"], "population_sd");
    assert_eq!(hold["status"], "pending");
    let hold_id = hold["id"].as_str().unwrap().to_string();

    let (stdev, estimator, source) = sample_row(&fx, T1).await;
    assert!(close(stdev.unwrap(), SAMPLE_SD), "served under the sample divisor: {stdev:?}");
    assert_eq!(estimator, "sample");
    assert_eq!(source, "default", "computed under no declaration");

    // --- The report lists it, with the evidence and no ruling ---
    let (status, body) = crate::common::get_with_token(
        &fx.app,
        "/api/actions/undeclared_sd_estimators",
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "undeclared report ({status}): {body}");
    let report: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(report["total_slots"], 1, "{report}");
    let slot = &report["slots"][0];
    assert_eq!(slot["population_signature_holds"], 1);
    assert_eq!(slot["source_reports_sd"], true, "the source ships an sd column");

    // --- The gate: three ways of accepting, all refused ---
    let (status, body) = resolve(&fx, &hold_id, &json!({"mode": "ours"})).await;
    assert_eq!(status, 409, "resolve ours must be gated: {body}");
    let message = body["message"].as_str().or_else(|| body["error"].as_str()).unwrap_or_default();
    assert!(
        message.contains("population") && message.contains("sample"),
        "the refusal must name both divisors: {body}"
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/acknowledge"),
        &json!({}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 409, "acknowledge must be gated: {body}");

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/sync/replicate_audit_holds/acknowledge_bulk",
        &json!({"source_system": "sdsrc"}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "bulk ({status}): {body}");
    assert_eq!(body["acknowledged"], 0, "the sweep must not reach it: {body}");
    assert_eq!(
        body["skipped_undeclared_estimator"], 1,
        "and must say what it left: {body}"
    );

    assert_eq!(
        hold_status(&fx.db, &hold_id).await,
        "pending",
        "still awaiting a decision after all three"
    );

    // --- Declaring it resolves the hold and recomputes the slot ---
    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "estimator", "estimator": "population", "scope": "slot"}),
    )
    .await;
    assert_eq!(status, 200, "declare ({status}): {body}");
    assert_eq!(body["status"], "remediated");
    assert!(
        crate::common::e2e::wait_for_jobs_by_trigger(&fx.db, "sd_estimator_retag", 30).await,
        "the retag job must complete"
    );

    assert_eq!(slot_declaration(&fx).await.as_deref(), Some("population"));
    let (stdev, estimator, source) = sample_row(&fx, T1).await;
    assert!(
        close(stdev.unwrap(), POPULATION_SD),
        "recomputed under the declared divisor: {stdev:?}"
    );
    assert_eq!(estimator, "population");
    assert_eq!(source, "slot");

    // --- The decision is on the chart, not only in the queue ---
    let notes = audit_annotations(&fx, &hold_id).await;
    assert_eq!(notes.len(), 1, "one point annotation per decision: {notes:?}");
    let (text, category, start) = &notes[0];
    assert_eq!(category, "audit");
    assert!(start.starts_with("2025-07-01 08:00"), "at the group's instant: {start}");
    assert!(
        text.contains("population") && text.contains("1.63") && text.contains('2'),
        "the note carries the decision and both numbers: {text}"
    );

    // The report no longer lists the slot: it has declared.
    let (_, body) = crate::common::get_with_token(
        &fx.app,
        "/api/actions/undeclared_sd_estimators",
        &fx.token,
    )
    .await;
    let report: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(report["total_slots"], 0, "{report}");

    // --- The comparison now agrees, so the same claim raises nothing ---
    ingest_population_claim(&fx, T2).await;
    let listed = holds(&fx, "&page_size=100").await;
    let at_t2 = listed["holds"]
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["group_time"].as_str().unwrap_or_default().starts_with(&T2[..19]));
    assert!(!at_t2, "a declared slot stops disagreeing: {listed}");

    // --- A real disagreement is still held, and now acceptable ---
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": fx.stream,
            "readings": group(T3),
            // A mean nobody's replicates produce: not the divisor, a genuine finding.
            "audit": [{"time": T3, "expected_mean": 40.0, "expected_sd": POPULATION_SD,
                       "expected_n": 3}],
        }),
        &fx.sync_token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    let mean_hold = hold_at(&fx, T3).await;
    assert_ne!(mean_hold["classification"], "population_sd");
    let mean_hold_id = mean_hold["id"].as_str().unwrap().to_string();
    let (status, body) = resolve(&fx, &mean_hold_id, &json!({"mode": "ours"})).await;
    assert_eq!(
        status, 200,
        "the gate is disarmed once the slot has declared: {body}"
    );

    // --- Reopen puts the declaration, the statistics and the gate back ---
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/reopen"),
        &json!({}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "reopen ({status}): {body}");
    assert_eq!(
        slot_declaration(&fx).await,
        None,
        "reverted to undeclared, not to a divisor nobody chose"
    );
    let (stdev, estimator, source) = sample_row(&fx, T1).await;
    assert!(close(stdev.unwrap(), SAMPLE_SD), "back to the sample divisor: {stdev:?}");
    assert_eq!(estimator, "sample");
    assert_eq!(source, "default");

    assert!(
        audit_annotations(&fx, &hold_id).await.is_empty(),
        "the note said a decision had been taken, and it has not any more"
    );

    let (status, _) = resolve(&fx, &hold_id, &json!({"mode": "ours"})).await;
    assert_eq!(status, 409, "and the gate is armed again");
}

#[tokio::test]
#[serial]
async fn an_instant_decision_leaves_the_parameter_undeclared_and_survives_a_retag() {
    let fx = setup().await;
    ingest_population_claim(&fx, T1).await;
    ingest_population_claim(&fx, T2).await;

    let hold_id = hold_at(&fx, T1).await["id"].as_str().unwrap().to_string();
    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "estimator", "estimator": "population", "scope": "instant"}),
    )
    .await;
    assert_eq!(status, 200, "declare for the instant ({status}): {body}");
    assert_eq!(body["samples_affected"], 1);

    // Only that group moved, and the parameter still has no declaration.
    assert_eq!(slot_declaration(&fx).await, None);
    let (stdev, estimator, source) = sample_row(&fx, T1).await;
    assert!(close(stdev.unwrap(), POPULATION_SD), "{stdev:?}");
    assert_eq!(estimator, "population");
    assert_eq!(source, "sample", "recorded as a decision about this instant");

    let (stdev, _, source) = sample_row(&fx, T2).await;
    assert!(close(stdev.unwrap(), SAMPLE_SD), "the other instant is untouched: {stdev:?}");
    assert_eq!(source, "default");

    // The slot's other hold is still gated: one instant's decision is not the parameter's.
    let other = hold_at(&fx, T2).await["id"].as_str().unwrap().to_string();
    let (status, _) = resolve(&fx, &other, &json!({"mode": "ours"})).await;
    assert_eq!(status, 409, "the parameter is still undeclared");

    // --- A slot-level retag does not stomp the instant decision ---
    let (status, body) = resolve(
        &fx,
        &other,
        &json!({"mode": "estimator", "estimator": "sample", "scope": "slot"}),
    )
    .await;
    assert_eq!(status, 200, "declare sample for the slot ({status}): {body}");
    crate::common::e2e::wait_for_jobs_by_trigger(&fx.db, "sd_estimator_retag", 30).await;

    let (stdev, estimator, source) = sample_row(&fx, T1).await;
    assert!(
        close(stdev.unwrap(), POPULATION_SD),
        "the instant decision survives a parameter-level retag: {stdev:?}"
    );
    assert_eq!(estimator, "population");
    assert_eq!(source, "sample");
}
