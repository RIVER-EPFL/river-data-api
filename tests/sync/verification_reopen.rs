//! Reopening a manager's ruling on an intern's entry or field day returns it to the review queue
//! as it stood before the ruling: the ruling's decisions are rolled back, the holds it closed are
//! pending again, and the next ruling acts on the restored state.
//!
//! Run with: cargo test --test sync verification_reopen -- --test-threads=1

use river_db::common::bulk_write;
use river_db::routes::private::readings::models::{Kind, Origin};
use river_db::routes::private::readings::service::{self as decisions, Decision, DecisionKey};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const AT: &str = "2025-06-15T10:00:00Z";
const VISIT_AT: &str = "2025-06-01T08:00:00Z";

struct Fixture {
    app: axum::Router,
    token: String,
    db: DatabaseConnection,
    stream: Uuid,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        &db,
        "reopen-temp",
        crate::common::PARAM_S1_TEMP_ID,
    )
    .await;
    Fixture {
        app,
        token,
        db,
        stream,
    }
}

/// Three replicates an intern entered at one slot instant, pending a ruling.
async fn seed_pending_entry(f: &Fixture) {
    for (i, v) in [1.0, 2.0, 3.0].iter().enumerate() {
        crate::common::exec(
            &f.db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
                 calibrated_value, replicate_index, measurement_type) \
                 VALUES ('{}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}', {v}, {v}, {i}, \
                         'spot')",
                f.stream
            ),
        )
        .await;
    }
    let entry = Decision {
        key: DecisionKey {
            stream_id: f.stream,
            time: AT.parse().expect("instant"),
            replicate_index: None,
        },
        kind: Kind::UnverifiedEntry,
        new: json!({}),
        actor: "intern".to_string(),
        reason: Some("entered".to_string()),
        origin: Origin::Manual,
        set_id: None,
    };
    bulk_write::guarded(&f.db, async |txn| decisions::record(txn, &entry).await)
        .await
        .expect("the entry is pending");
}

async fn insert_hold(
    db: &DatabaseConnection,
    kind: &str,
    parameter: Option<&str>,
    time: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    let parameter = parameter.map_or_else(|| "NULL".to_string(), |p| format!("'{p}'"));
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO replicate_audit_holds \
                 (id, stream_id, site_id, parameter_id, group_time, kind, expected, computed, \
                  delta, status) \
             VALUES ('{id}', NULL, '{SITE1_ID}', {parameter}, '{time}', '{kind}', \
                     '{{\"state\": \"verified\"}}'::jsonb, \
                     '{{\"state\": \"unverified\"}}'::jsonb, '{{}}'::jsonb, 'pending')"
        ),
    )
    .await;
    id
}

async fn post(f: &Fixture, hold: Uuid, action: &str, body: &serde_json::Value) -> (u16, String) {
    crate::common::post_json_with_token(
        &f.app,
        &format!("/api/sync/replicate_audit_holds/{hold}/{action}"),
        body,
        &f.token,
    )
    .await
}

async fn rule(f: &Fixture, hold: Uuid, mode: &str) -> (u16, String) {
    post(f, hold, "resolve", &json!({ "mode": mode })).await
}

async fn reopen(f: &Fixture, hold: Uuid) -> (u16, String) {
    post(f, hold, "reopen", &json!({})).await
}

async fn scalar(db: &DatabaseConnection, sql: &str) -> i64 {
    crate::common::e2e::count(db, sql).await
}

async fn hold_status(db: &DatabaseConnection, hold: Uuid) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT status FROM replicate_audit_holds WHERE id = '{hold}'"),
    ))
    .await
    .expect("query")
    .expect("the hold")
    .try_get("", "status")
    .expect("status")
}

/// Each replicate of the entry as (pending, withdrawn).
async fn entry_state(f: &Fixture) -> Vec<(bool, bool)> {
    f.db.query_all_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT unverified, withdrawn_at IS NOT NULL AS withdrawn FROM readings \
             WHERE stream_id = '{}' AND time = '{AT}' ORDER BY replicate_index",
            f.stream
        ),
    ))
    .await
    .expect("query")
    .iter()
    .map(|r| {
        (
            r.try_get("", "unverified").expect("unverified"),
            r.try_get("", "withdrawn").expect("withdrawn"),
        )
    })
    .collect()
}

/// The visit at `VISIT_AT` as (id, pending, withdrawn).
async fn visit_state(db: &DatabaseConnection) -> (Uuid, bool, bool) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT id, unverified, withdrawn_at IS NOT NULL AS withdrawn \
                   FROM collection_events \
                  WHERE site_id = '{SITE1_ID}' AND collected_at = '{VISIT_AT}'"
            ),
        ))
        .await
        .expect("query")
        .expect("the visit stands");
    (
        row.try_get("", "id").expect("id"),
        row.try_get("", "unverified").expect("unverified"),
        row.try_get("", "withdrawn").expect("withdrawn"),
    )
}

/// A field day an intern opened with two measurements, each with its own hold, and the visit's
/// hold pending a manager's ruling.
async fn seed_pending_visit(f: &Fixture) -> (Uuid, Uuid, [Uuid; 2]) {
    let (status, body) = crate::common::post_json_with_token(
        &f.app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 9.0, "time": VISIT_AT },
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 4.2, "time": VISIT_AT },
            ],
        }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (visit, _, _) = visit_state(&f.db).await;
    crate::common::exec(
        &f.db,
        &format!("UPDATE collection_events SET unverified = TRUE WHERE id = '{visit}'"),
    )
    .await;
    let hold = insert_hold(&f.db, "unverified_visit", None, VISIT_AT).await;
    let entries = [
        insert_hold(
            &f.db,
            "unverified_entry",
            Some(GLOBAL_PARAM_DO_ID),
            VISIT_AT,
        )
        .await,
        insert_hold(
            &f.db,
            "unverified_entry",
            Some(GLOBAL_PARAM_TEMP_ID),
            VISIT_AT,
        )
        .await,
    ];
    (visit, hold, entries)
}

async fn live_visit_readings(db: &DatabaseConnection, visit: Uuid) -> i64 {
    scalar(
        db,
        &format!(
            "SELECT count(*) AS c FROM readings \
              WHERE collection_event_id = '{visit}' AND withdrawn_at IS NULL"
        ),
    )
    .await
}

/// Scenario: a manager verifies an intern's entry, then reopens the ruling.
///
/// Expected behaviour: the entry is pending again and back in the queue, and a reject taken after
/// the reopen withdraws it.
#[tokio::test]
#[serial]
async fn reopening_a_verified_entry_returns_it_to_pending() {
    let f = setup().await;
    seed_pending_entry(&f).await;
    let hold = insert_hold(&f.db, "unverified_entry", Some(GLOBAL_PARAM_TEMP_ID), AT).await;

    let (status, body) = rule(&f, hold, "verify").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(entry_state(&f).await, vec![(false, false); 3]);

    let (status, body) = reopen(&f, hold).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"pending\""), "{body}");
    assert_eq!(hold_status(&f.db, hold).await, "pending");
    assert_eq!(
        entry_state(&f).await,
        vec![(true, false); 3],
        "the verify is undone, so the entry is pending again"
    );
    assert_eq!(
        scalar(
            &f.db,
            &format!(
                "SELECT count(*) AS c FROM reading_decisions \
                  WHERE stream_id = '{}' AND kind = 'verify' AND rolled_back_by IS NULL",
                f.stream
            ),
        )
        .await,
        0,
        "the verify stays on the record, rolled back"
    );

    let (status, body) = rule(&f, hold, "reject").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"samples_affected\":3"), "{body}");
    assert_eq!(
        entry_state(&f).await,
        vec![(false, true); 3],
        "the reject after the reopen withdraws the entry"
    );
}

/// Scenario: a manager rejects an intern's entry, then reopens the ruling.
///
/// Expected behaviour: the entry is restored and pending again, and can then be verified.
#[tokio::test]
#[serial]
async fn reopening_a_rejected_entry_restores_it_pending() {
    let f = setup().await;
    seed_pending_entry(&f).await;
    let hold = insert_hold(&f.db, "unverified_entry", Some(GLOBAL_PARAM_TEMP_ID), AT).await;

    let (status, body) = rule(&f, hold, "reject").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(entry_state(&f).await, vec![(false, true); 3]);

    let (status, body) = reopen(&f, hold).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hold_status(&f.db, hold).await, "pending");
    assert_eq!(
        entry_state(&f).await,
        vec![(true, false); 3],
        "the withdrawal is undone and the entry awaits a ruling again"
    );

    let (status, body) = rule(&f, hold, "verify").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(entry_state(&f).await, vec![(false, false); 3]);
}

/// Scenario: an entry's hold was ruled on again at the same slot instant after a new entry raised
/// a fresh hold there.
///
/// Expected behaviour: reopening the older ruling is refused, since the slot already has a hold
/// in the queue, and nothing is rolled back.
#[tokio::test]
#[serial]
async fn reopening_an_entry_is_refused_while_its_slot_has_a_pending_hold() {
    let f = setup().await;
    seed_pending_entry(&f).await;
    let hold = insert_hold(&f.db, "unverified_entry", Some(GLOBAL_PARAM_TEMP_ID), AT).await;
    let (status, body) = rule(&f, hold, "verify").await;
    assert_eq!(status, 200, "{body}");
    let fresh = insert_hold(&f.db, "unverified_entry", Some(GLOBAL_PARAM_TEMP_ID), AT).await;

    let (status, body) = reopen(&f, hold).await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains(&fresh.to_string()),
        "the refusal names it: {body}"
    );
    assert_eq!(hold_status(&f.db, hold).await, "acknowledged");
    assert_eq!(entry_state(&f).await, vec![(false, false); 3]);
}

/// Scenario: a verification hold decided before its ruling's decisions were recorded as a set.
///
/// Expected behaviour: the reopen cannot tell which decisions to roll back, so it refuses and
/// names the route that reverts one decision at a time.
#[tokio::test]
#[serial]
async fn reopening_a_ruling_with_no_recorded_set_is_refused() {
    let f = setup().await;
    let hold = insert_hold(&f.db, "unverified_entry", Some(GLOBAL_PARAM_TEMP_ID), AT).await;
    crate::common::exec(
        &f.db,
        &format!(
            "UPDATE replicate_audit_holds SET status = 'acknowledged', \
                    resolution = '{{\"mode\": \"verify\", \"by\": \"manager\", \"rows\": 3}}' \
              WHERE id = '{hold}'"
        ),
    )
    .await;

    let (status, body) = reopen(&f, hold).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("/api/readings/edits/"), "{body}");
    assert_eq!(hold_status(&f.db, hold).await, "acknowledged");
}

/// Scenario: a manager rejects an intern's field day, then reopens the ruling.
///
/// Expected behaviour: the visit, its readings and each measurement's hold are as they were before
/// the reject: the visit pending and not withdrawn, the readings live, the holds in the queue.
#[tokio::test]
#[serial]
async fn reopening_a_rejected_visit_restores_the_field_day() {
    let f = setup().await;
    let (visit, hold, entries) = seed_pending_visit(&f).await;
    assert_eq!(live_visit_readings(&f.db, visit).await, 2);

    let (status, body) = rule(&f, hold, "reject").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(live_visit_readings(&f.db, visit).await, 0);
    for entry in entries {
        assert_eq!(hold_status(&f.db, entry).await, "remediated");
    }

    let (status, body) = reopen(&f, hold).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hold_status(&f.db, hold).await, "pending");
    let (_, unverified, withdrawn) = visit_state(&f.db).await;
    assert!(unverified, "the field day awaits a ruling again");
    assert!(!withdrawn, "the rejection's stamp is lifted");
    assert_eq!(
        live_visit_readings(&f.db, visit).await,
        2,
        "every reading the reject withdrew is restored"
    );
    for entry in entries {
        assert_eq!(
            hold_status(&f.db, entry).await,
            "pending",
            "each measurement is owed its ruling again"
        );
    }
}

/// Scenario: a manager verifies an intern's field day and reopens it, and in another run verifies
/// a measurement at it before reopening.
///
/// Expected behaviour: with nothing ruled at the visit since, the visit is pending again. Once a
/// measurement has been ruled on, reopening the visit is refused: a pending field day cannot hold
/// a verified measurement.
#[tokio::test]
#[serial]
async fn reopening_a_verified_visit_returns_it_to_pending_unless_a_measurement_was_ruled() {
    let f = setup().await;
    let (_, hold, entries) = seed_pending_visit(&f).await;

    let (status, body) = rule(&f, hold, "verify").await;
    assert_eq!(status, 200, "{body}");
    assert!(!visit_state(&f.db).await.1);
    let (status, body) = reopen(&f, hold).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(hold_status(&f.db, hold).await, "pending");
    assert!(visit_state(&f.db).await.1, "the field day is pending again");

    let (status, body) = rule(&f, hold, "verify").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = rule(&f, entries[0], "verify").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = reopen(&f, hold).await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains(&entries[0].to_string()),
        "the refusal names it: {body}"
    );
    assert_eq!(hold_status(&f.db, hold).await, "acknowledged");
    assert!(!visit_state(&f.db).await.1, "the visit stays verified");
}
