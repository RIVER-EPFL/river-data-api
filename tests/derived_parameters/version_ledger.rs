//! Scenario: a calculation computes on a stream under one version, its set is saved again, and the
//! stored values are recomputed under the new one.
//!
//! Expected behaviour: the calculation's version ledger lists a row per version, each carrying the
//! readings that version made and the span they cover (Q232). A stream pass mints no run, so this
//! is read off the curation ledger rather than from a run table.
//!
//! Run with: cargo test --test derived_parameters version_ledger

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

const POLL_DEADLINE_SECS: u64 = 30;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

/// Save the calculation's whole set, the one act that mints a version (Q186).
async fn save_set(
    app: &axum::Router,
    token: &str,
    calculation: Uuid,
    code: &str,
    formula_id: Option<Uuid>,
    formula: &str,
) {
    let mut row = json!({
        "code": code,
        "name": "Version ledger fixture",
        "units": "mg/L",
        "formula": formula,
        "ordinal": 0,
    });
    if let Some(id) = formula_id {
        row["id"] = json!(id);
    }
    let (status, body) = crate::common::post_json_with_token(
        app,
        &format!("/api/tool_scripts/{calculation}/formulas"),
        &json!({ "formulas": [row], "migrate_stored": true }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "save set ({status}): {body}");
}

async fn saved_formula(db: &DatabaseConnection, code: &str) -> (Uuid, Uuid) {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, output_parameter_id FROM calculation_formulas \
              WHERE LOWER(code) = LOWER($1)",
            [code.into()],
        ))
        .await
        .expect("a query")
        .expect("the save wrote the formula");
    (
        row.try_get("", "id").expect("an id"),
        row.try_get("", "output_parameter_id")
            .expect("an output parameter"),
    )
}

/// Wait until the slot serves `value`, so a recompute's effect is read rather than raced.
async fn wait_for_value(db: &DatabaseConnection, parameter: Uuid, at: DateTime<Utc>, value: f64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_DEADLINE_SECS);
    let mut held = None;
    while std::time::Instant::now() < deadline {
        held = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT raw_value FROM readings WHERE parameter_id = $1 AND time = $2",
                [parameter.into(), at.into()],
            ))
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<Option<f64>>("", "raw_value").ok())
            .flatten();
        if held == Some(value) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("the slot never reached {value}; it holds {held:?}");
}

/// One of a row's timestamps, as the instant it names. The API spells UTC `Z` and `to_rfc3339`
/// spells it `+00:00`, so the two are compared as times rather than as text.
fn instant(row: &Value, key: &str) -> Option<DateTime<Utc>> {
    row[key]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
}

async fn ledger(app: &axum::Router, token: &str, calculation: Uuid) -> Vec<Value> {
    let (status, body) = crate::common::get_json_with_token(
        app,
        &format!("/api/tool_scripts/{calculation}/version_ledger"),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body.as_array().expect("a ledger").clone()
}

#[tokio::test]
#[serial]
async fn each_version_lists_the_readings_it_computed_and_the_span_they_cover() {
    let (db, app, token) = setup().await;

    let code = format!("vledger_{}", Uuid::new_v4().simple());
    let calculation = crate::common::seed_formula_calculation(&db, &format!("{code}_set")).await;
    save_set(
        &app,
        &token,
        calculation,
        &code,
        None,
        "Dissolved_O2 * 0.032",
    )
    .await;
    let (formula_id, parameter) = saved_formula(&db, &code).await;

    // A version that has computed nothing is a row of zeros, not an absence. Read before the slot
    // exists, because assigning one backfills whatever the site already holds.
    let before = ledger(&app, &token, calculation).await;
    assert_eq!(before.len(), 1, "one version so far: {before:?}");
    assert_eq!(before[0]["readings"], 0, "{before:?}");
    assert!(before[0]["first_instant"].is_null(), "{before:?}");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": parameter,
            "name": code,
            "sensor_type": "derived",
            "entry_mode": "tool",
            "cadence": "high",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "assign ({status}): {body}");

    let at: DateTime<Utc> = Utc::now() - Duration::hours(8);
    let at = at - Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()));
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
                "time": at.to_rfc3339(),
                "raw_value": 250.0,
            }]
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "ingest ({status}): {body}");
    wait_for_value(&db, parameter, at, 8.0).await; // 250 * 0.032

    let one = ledger(&app, &token, calculation).await;
    assert_eq!(one.len(), 1, "still one version: {one:?}");
    let made = one[0]["readings"].as_i64().expect("a count");
    assert!(made >= 1, "the version names what it made: {one:?}");
    assert!(
        one[0]["first_computed"].is_string() && one[0]["last_instant"].is_string(),
        "the row spans both the instants and the computing: {one:?}"
    );
    assert_eq!(
        instant(&one[0], "last_instant"),
        Some(at),
        "the newest instant is the one just ingested: {one:?}"
    );

    // A second save mints a version and, on the correcting arm, repairs what the first left on
    // both arms: the stream value moves onto the new version with no recompute asked for by hand.
    save_set(
        &app,
        &token,
        calculation,
        &code,
        Some(formula_id),
        "Dissolved_O2 * 0.064",
    )
    .await;
    wait_for_value(&db, parameter, at, 16.0).await; // 250 * 0.064

    let two = ledger(&app, &token, calculation).await;
    assert_eq!(two.len(), 2, "a row per version, newest first: {two:?}");
    assert_eq!(two[0]["version_no"], 2);
    assert_eq!(two[1]["version_no"], 1);
    assert!(
        two[0]["readings"].as_i64().expect("a count") >= 1,
        "the new version moved what it recomputed: {two:?}"
    );
    assert_eq!(
        two[1]["readings"].as_i64(),
        Some(made),
        "the version that made them keeps its own row: {two:?}"
    );
    assert_eq!(
        two[0]["last_instant"], two[1]["last_instant"],
        "both name the instants the values sit at: {two:?}"
    );
}
