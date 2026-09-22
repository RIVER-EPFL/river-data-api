//! Scenario: a formula is edited and the derived values it made are recomputed.
//!
//! Expected behaviour: each value that moves records one `formula_transition` decision naming the
//! value and the version on both sides (Q116, M135), and a recompute that moves nothing records
//! nothing, so the ledger grows with edits rather than with passes (T83).

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const POLL_DEADLINE_SECS: u64 = 30;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

/// The stored derived value at the slot, polled until the compute lands.
async fn derived_value(
    db: &DatabaseConnection,
    parameter_id: Uuid,
    time: DateTime<Utc>,
) -> Option<f64> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_DEADLINE_SECS);
    while std::time::Instant::now() < deadline {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT raw_value FROM readings WHERE parameter_id = $1 AND time = $2 LIMIT 1",
                [parameter_id.into(), time.into()],
            ))
            .await
            .ok()
            .flatten();
        if let Some(r) = row
            && let Ok(Some(v)) = r.try_get::<Option<f64>>("", "raw_value")
        {
            return Some(v);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

/// Every transition decision at one instant of the slot, oldest first, as `(old, new)` blobs.
async fn transitions(
    db: &DatabaseConnection,
    parameter_id: Uuid,
    time: DateTime<Utc>,
) -> Vec<(serde_json::Value, serde_json::Value)> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.old, d.new FROM reading_decisions d \
               JOIN readings r ON r.stream_id = d.stream_id AND r.time = d.time \
              WHERE d.kind = 'formula_transition' AND r.parameter_id = $1 AND d.time = $2 \
              ORDER BY d.at, d.id",
            [parameter_id.into(), time.into()],
        ))
        .await
        .expect("transitions");
    rows.iter()
        .map(|r| {
            (
                r.try_get::<serde_json::Value>("", "old").expect("old"),
                r.try_get::<serde_json::Value>("", "new").expect("new"),
            )
        })
        .collect()
}

/// The `derived_computed` decisions at one instant of the slot, as their `new` blobs.
async fn arrivals(
    db: &DatabaseConnection,
    parameter_id: Uuid,
    time: DateTime<Utc>,
) -> Vec<serde_json::Value> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.new FROM reading_decisions d \
               JOIN readings r ON r.stream_id = d.stream_id AND r.time = d.time \
              WHERE d.kind = 'derived_computed' AND r.parameter_id = $1 AND d.time = $2 \
              ORDER BY d.seq",
            [parameter_id.into(), time.into()],
        ))
        .await
        .expect("arrivals");
    rows.iter()
        .map(|r| r.try_get::<serde_json::Value>("", "new").expect("new"))
        .collect()
}

/// The consumed entry for one variable of a decision's `new` blob.
fn consumed<'a>(new: &'a serde_json::Value, variable: &str) -> &'a serde_json::Value {
    new["consumed"]
        .as_array()
        .unwrap_or_else(|| panic!("the decision names what it consumed: {new}"))
        .iter()
        .find(|c| c["variable"] == variable)
        .unwrap_or_else(|| panic!("{variable} was consumed: {new}"))
}

/// Wait until the transition count stops being `before`, or give up. The recompute is a tracked
/// job, so the decision lands after the endpoint answers.
async fn wait_for_transitions(
    db: &DatabaseConnection,
    parameter_id: Uuid,
    time: DateTime<Utc>,
    before: usize,
) -> Vec<(serde_json::Value, serde_json::Value)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_DEADLINE_SECS);
    loop {
        let rows = transitions(db, parameter_id, time).await;
        if rows.len() != before || std::time::Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Save a calculation's whole set, the one act that mints a version (Q186). An `id` updates the
/// stored formula rather than replacing it, so its output parameter and its readings stay.
async fn save_set(
    app: &axum::Router,
    token: &str,
    calculation: Uuid,
    code: &str,
    formula_id: Option<Uuid>,
    formula: &str,
) {
    let mut row = serde_json::json!({
        "code": code,
        "name": "Formula transition fixture",
        "units": "mg/L",
        "formula": formula,
        "ordinal": 0,
    });
    if let Some(id) = formula_id {
        row["id"] = serde_json::json!(id);
    }
    let (status, body) = crate::common::post_json_with_token(
        app,
        &format!("/api/tool_scripts/{calculation}/formulas"),
        &serde_json::json!({ "formulas": [row] }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "save set ({status}): {body}");
}

/// The formula row a save wrote, and the parameter it outputs.
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

#[tokio::test]
#[serial]
async fn a_recompute_records_the_move_and_a_pass_that_moves_nothing_records_nothing() {
    let (db, app, token) = setup().await;

    let code = format!("ftrans_{}", Uuid::new_v4().simple());
    let calculation = crate::common::seed_formula_calculation(&db, &format!("{code}_set")).await;
    // The set-level save is the one act that mints a version (Q186), and a stored value names the
    // version that made it, so the move this story is about is a move between two of them.
    save_set(&app, &token, calculation, &code, None, "Dissolved_O2 * 0.032").await;
    let (definition_id, output) = saved_formula(&db, &code).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": output,
            "name": code,
            "sensor_type": "derived",
            "entry_mode": "tool",
            "cadence": "high",
            "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "assign ({status}): {body}");

    let at: DateTime<Utc> = Utc::now() - Duration::hours(9);
    let at = at - Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()));
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &serde_json::json!({
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

    let parameter = output;
    assert_eq!(
        derived_value(&db, parameter, at).await,
        Some(8.0), // 250.0 * 0.032
        "the first computation lands"
    );
    assert!(
        transitions(&db, parameter, at).await.is_empty(),
        "a first computation came from no version, so it is not a transition"
    );
    // The arrival names what it consumed: the input at its arrival state, and the formula row
    // at the revision the audit trail holds (Q215).
    let born = arrivals(&db, parameter, at).await;
    assert_eq!(born.len(), 1, "one arrival: {born:?}");
    let input = consumed(&born[0], "Dissolved_O2");
    assert_eq!(input["kind"], "reading", "{born:?}");
    assert_eq!(input["members"][0]["value"].as_f64(), Some(250.0));
    assert!(input["members"][0]["stream_id"].is_string(), "{born:?}");
    assert!(
        input["members"][0]["revision"].is_null(),
        "untouched, so at its arrival state: {born:?}"
    );
    let step = consumed(&born[0], &code);
    assert_eq!(step["kind"], "step");
    assert_eq!(
        step["subject"],
        serde_json::json!(format!("calculation_formula:{definition_id}"))
    );
    let step_revision_at_birth = step["revision"]
        .as_i64()
        .expect("the formula row has a revision");

    save_set(
        &app,
        &token,
        calculation,
        &code,
        Some(definition_id),
        "Dissolved_O2 * 0.064",
    )
    .await;

    let uri = format!("/api/actions/derived_parameters/{calculation}/recompute");
    let (status, body) =
        crate::common::post_json_with_token(&app, &uri, &serde_json::json!({}), &token).await;
    assert!((200..300).contains(&status), "recompute ({status}): {body}");

    let moved = wait_for_transitions(&db, parameter, at, 0).await;
    assert_eq!(moved.len(), 1, "the move is recorded once: {moved:?}");
    let (old, new) = &moved[0];
    // jsonb normalises 8.0 to 8, so the numbers are compared as numbers.
    assert_eq!(old["raw_value"].as_f64(), Some(8.0));
    assert_eq!(new["raw_value"].as_f64(), Some(16.0)); // 250.0 * 0.064
    assert!(
        !old["derived_version_id"].is_null() && !new["derived_version_id"].is_null(),
        "both sides name a version: {old} -> {new}"
    );
    assert_ne!(
        old["derived_version_id"], new["derived_version_id"],
        "the versions differ, which is the move"
    );
    // The move names what it consumed, and the edited formula row is at a later revision.
    let step = consumed(new, &code);
    assert!(
        step["revision"].as_i64().expect("a revision") > step_revision_at_birth,
        "the edit advanced the formula's revision: {new}"
    );
    assert_eq!(
        consumed(new, "Dissolved_O2")["members"][0]["value"].as_f64(),
        Some(250.0)
    );

    // A second recompute with nothing edited moves nothing, so it decides nothing.
    let (status, body) =
        crate::common::post_json_with_token(&app, &uri, &serde_json::json!({}), &token).await;
    assert!((200..300).contains(&status), "recompute ({status}): {body}");
    let after = wait_for_transitions(&db, parameter, at, 1).await;
    assert_eq!(
        after.len(),
        1,
        "a recompute that changes no value writes no decision: {after:?}"
    );
}

/// The stream a slot's derived readings are stored under.
async fn derived_stream(db: &DatabaseConnection, parameter_id: Uuid, time: DateTime<Utc>) -> Uuid {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT stream_id FROM readings WHERE parameter_id = $1 AND time = $2 LIMIT 1",
        [parameter_id.into(), time.into()],
    ))
    .await
    .expect("the reading reads")
    .expect("a derived reading")
    .try_get::<Uuid>("", "stream_id")
    .expect("stream_id")
}

/// Scenario: a reader is shown a derived value beside the input it consumed, and wants to check
/// one against the other.
///
/// Expected behaviour: the replay runs the formula the computation recorded over the values it
/// recorded, and answers the same number that is stored. It reads the captured set and nothing
/// else, so flagging the input afterwards does not move it.
#[tokio::test]
#[serial]
async fn a_derived_value_replays_its_own_formula_over_what_it_consumed() {
    let (db, app, token) = setup().await;

    let code = format!("replay_{}", Uuid::new_v4().simple());
    let calculation = crate::common::seed_formula_calculation(&db, &format!("{code}_set")).await;
    let (status, def) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": code,
            "name": "Replay fixture",
            "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
            "tool_script_id": calculation,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {def}");
    let output = def["output_parameter_id"]
        .as_str()
        .expect("output")
        .to_string();

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": output,
            "name": code,
            "sensor_type": "derived",
            "entry_mode": "tool",
            "cadence": "high",
            "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "assign ({status}): {body}");

    let at: DateTime<Utc> = Utc::now() - Duration::hours(11);
    let at = at - Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()));
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &serde_json::json!({
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

    let parameter = Uuid::parse_str(&output).unwrap();
    assert_eq!(derived_value(&db, parameter, at).await, Some(8.0));
    let stream = derived_stream(&db, parameter, at).await;

    let uri = format!(
        "/api/readings/replay?stream_id={stream}&time={}&replicate_index=0",
        at.to_rfc3339().replace('+', "%2B")
    );
    let (status, replay) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "replay ({status}): {replay}");
    assert_eq!(replay["formula"], "Dissolved_O2 * 0.032");
    // 250.0 * 0.032
    assert_eq!(replay["replayed"].as_f64(), Some(8.0));
    assert_eq!(
        replay["stored"].as_f64(),
        Some(8.0),
        "the arithmetic answers the number standing there: {replay}"
    );
    assert_eq!(
        replay["variables"]["Dissolved_O2"].as_f64(),
        Some(250.0),
        "the value the computation read, not the one the store holds now: {replay}"
    );
}

/// Scenario: an instant with no computation recorded against it.
///
/// Expected behaviour: the reader is told there is nothing to replay rather than shown a number
/// made up from the values as they stand.
#[tokio::test]
#[serial]
async fn an_instant_with_no_computation_has_nothing_to_replay() {
    let (db, app, token) = setup().await;
    let at: DateTime<Utc> = Utc::now() - Duration::hours(12);
    let at = at - Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()));
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &serde_json::json!({
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
    let stream = derived_stream(
        &db,
        Uuid::parse_str(crate::common::GLOBAL_PARAM_DO_ID).unwrap(),
        at,
    )
    .await;

    let uri = format!(
        "/api/readings/replay?stream_id={stream}&time={}&replicate_index=0",
        at.to_rfc3339().replace('+', "%2B")
    );
    let (status, body) = crate::common::get_with_token(&app, &uri, &token).await;
    assert_eq!(status, 404, "{body}");
}
