//! A manager's ruling on an intern's entry records its decisions as one set named on the hold, and
//! is undone only by reopening that hold, which also returns the hold to the review queue. The
//! edit rollbacks refuse the ruling's set and each of its decisions, naming the reopen.
//!
//! Run with: cargo test --test readings ruling_rollback -- --test-threads=1

use river_db::common::bulk_write;
use river_db::routes::private::readings::models::{Kind, Origin};
use river_db::routes::private::readings::service::{self as decisions, Decision, DecisionKey};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const AT: &str = "2025-06-15T10:00:00Z";

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
        "ruling-rollback-temp",
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

/// Three replicates an intern entered at one slot instant, with the hold a manager rules on.
async fn seed_pending_entry(f: &Fixture) -> Uuid {
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
    let hold = Uuid::new_v4();
    crate::common::exec(
        &f.db,
        &format!(
            "INSERT INTO replicate_audit_holds \
                 (id, stream_id, site_id, parameter_id, group_time, kind, expected, computed, \
                  delta, status) \
             VALUES ('{hold}', NULL, '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}', \
                     'unverified_entry', '{{\"state\": \"verified\"}}'::jsonb, \
                     '{{\"state\": \"unverified\"}}'::jsonb, '{{}}'::jsonb, 'pending')"
        ),
    )
    .await;
    hold
}

async fn post(f: &Fixture, uri: &str) -> (u16, String) {
    crate::common::post_json_with_token(&f.app, uri, &json!({}), &f.token).await
}

/// Verify the entry and return the decision set the ruling recorded.
async fn verify(f: &Fixture, hold: Uuid) -> Uuid {
    let (status, body) = crate::common::post_json_with_token(
        &f.app,
        &format!("/api/sync/replicate_audit_holds/{hold}/resolve"),
        &json!({ "mode": "verify" }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let set: String =
        f.db.query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT resolution->>'set_id' AS s FROM replicate_audit_holds WHERE id = '{hold}'"
            ),
        ))
        .await
        .expect("query")
        .expect("the hold")
        .try_get("", "s")
        .expect("the ruling names its set");
    set.parse().expect("a set id")
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

async fn pending_replicates(f: &Fixture) -> i64 {
    crate::common::e2e::count(
        &f.db,
        &format!(
            "SELECT count(*) AS c FROM readings \
              WHERE stream_id = '{}' AND time = '{AT}' AND unverified",
            f.stream
        ),
    )
    .await
}

async fn a_verify_decision(f: &Fixture, set: Uuid) -> Uuid {
    f.db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT id FROM reading_decisions WHERE set_id = '{set}' AND kind = 'verify' \
             ORDER BY replicate_index LIMIT 1"
        ),
    ))
    .await
    .expect("query")
    .expect("the ruling recorded a verify")
    .try_get("", "id")
    .expect("id")
}

/// Scenario: a manager verified an intern's entry, and the ruling's set is undone from a point's
/// inspector.
///
/// Expected behaviour: 409 naming the hold's reopen; the entry stays verified and the hold
/// decided, and the reopen still rolls the ruling back.
#[tokio::test]
#[serial]
async fn rolling_back_a_rulings_set_is_refused_naming_the_reopen() {
    let f = setup().await;
    let hold = seed_pending_entry(&f).await;
    let set = verify(&f, hold).await;

    let (status, body) = post(&f, &format!("/api/readings/edits/sets/{set}/rollback")).await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains(&format!("/api/sync/replicate_audit_holds/{hold}/reopen")),
        "the refusal names the reopen: {body}"
    );
    assert_eq!(pending_replicates(&f).await, 0, "the entry stays verified");
    assert_eq!(hold_status(&f.db, hold).await, "acknowledged");

    let (status, body) = post(
        &f,
        &format!("/api/sync/replicate_audit_holds/{hold}/reopen"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(pending_replicates(&f).await, 3);
    assert_eq!(hold_status(&f.db, hold).await, "pending");
}

/// Scenario: one verify decision of a manager's ruling is undone on its own.
///
/// Expected behaviour: 409 naming the hold's reopen, and the replicate stays verified.
#[tokio::test]
#[serial]
async fn rolling_back_one_decision_of_a_ruling_is_refused_naming_the_reopen() {
    let f = setup().await;
    let hold = seed_pending_entry(&f).await;
    let set = verify(&f, hold).await;
    let decision = a_verify_decision(&f, set).await;

    let (status, body) = post(&f, &format!("/api/readings/edits/{decision}/rollback")).await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains(&format!("/api/sync/replicate_audit_holds/{hold}/reopen")),
        "the refusal names the reopen: {body}"
    );
    assert_eq!(
        pending_replicates(&f).await,
        0,
        "the replicate stays verified"
    );
    assert_eq!(hold_status(&f.db, hold).await, "acknowledged");
}

/// Scenario: the decision history of a verified entry is read, before and after its ruling is
/// reopened.
///
/// Expected behaviour: each decision of the standing ruling names the hold it is reopened from,
/// and once reopened none does.
#[tokio::test]
#[serial]
async fn the_history_names_the_hold_a_ruling_is_reopened_from() {
    let f = setup().await;
    let hold = seed_pending_entry(&f).await;
    let set = verify(&f, hold).await;
    let uri = format!(
        "/api/readings/decisions?stream_id={}&time={}",
        f.stream,
        AT.replace(':', "%3A")
    );

    let (status, rows) = crate::common::get_json_with_token(&f.app, &uri, &f.token).await;
    assert_eq!(status, 200, "{rows}");
    let rows = rows.as_array().expect("a list");
    let hold_text = hold.to_string();
    let set_text = set.to_string();
    let ruled: Vec<_> = rows
        .iter()
        .filter(|r| r["set_id"].as_str() == Some(set_text.as_str()))
        .collect();
    assert!(
        !ruled.is_empty(),
        "the ruling's decisions are listed: {rows:?}"
    );
    for r in &ruled {
        assert_eq!(
            r["ruling_hold_id"].as_str(),
            Some(hold_text.as_str()),
            "{r}"
        );
    }
    let entered = rows
        .iter()
        .find(|r| r["kind"] == "unverified_entry")
        .expect("the entry's own decision");
    assert!(entered["ruling_hold_id"].is_null(), "{entered}");

    let (status, body) = post(
        &f,
        &format!("/api/sync/replicate_audit_holds/{hold}/reopen"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (_, rows) = crate::common::get_json_with_token(&f.app, &uri, &f.token).await;
    for r in rows.as_array().expect("a list") {
        assert!(
            r["ruling_hold_id"].is_null(),
            "a reopened ruling stands no more: {r}"
        );
    }
}
