//! S8, windowed reconciliation (story catalog: ../archived-documentation/PLAN.md).
//!
//! Scenario: a sync service re-reads its mutable source in full and asserts a completeness
//! window. The store converges: a removed replicate is withdrawn (a stamp, never a delete), an
//! unchanged re-send is a recorded no-op, and a value the source has changed since river-data
//! stored it is proposed rather than written, applied only when a person accepts it (Q84). A
//! reading an operator has flagged never changes servedness without a person, so its withdrawal
//! is held in the review queue instead. Dishonest windows are refused outright, and a pass
//! reshaping the window at scale is braked.

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-06-01T08:00:00Z";

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
    sync_token: String,
    stream_id: String,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "stn:WDiff:reps", "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{stream}");
    let stream_id = crate::common::e2e::id_of(&stream);
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_DO_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    Fixture {
        db,
        app,
        token,
        sync_token,
        stream_id,
    }
}

fn window(rows: u64) -> serde_json::Value {
    json!({ "from": "2025-01-01T00:00:00Z", "to": "2026-01-01T00:00:00Z", "source_rows_read": rows })
}

fn replicates(time: &str, values: &[(i16, f64)]) -> Vec<serde_json::Value> {
    values
        .iter()
        .map(|(i, v)| json!({ "time": time, "raw_value": v, "replicate_index": i }))
        .collect()
}

async fn windowed_ingest(
    fx: &Fixture,
    readings: Vec<serde_json::Value>,
    rows: u64,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": fx.stream_id,
            "collection": true,
            "window": window(rows),
            "readings": readings,
        }),
        &fx.sync_token,
    )
    .await
}

async fn windowed_ingest_digest(
    fx: &Fixture,
    readings: Vec<serde_json::Value>,
    rows: u64,
    digest: &str,
) -> (u16, serde_json::Value) {
    let mut w = window(rows);
    w["content_digest"] = json!(digest);
    crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": fx.stream_id,
            "collection": true,
            "window": w,
            "readings": readings,
        }),
        &fx.sync_token,
    )
    .await
}

async fn stored_digest(db: &DatabaseConnection, stream_id: &str) -> Option<String> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT last_window_digest FROM data_streams WHERE id = '{stream_id}'"),
    ))
    .await
    .unwrap()
    .expect("the stream exists")
    .try_get("", "last_window_digest")
    .unwrap()
}

async fn sample_stats(db: &DatabaseConnection) -> (f64, i32) {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT mean, n FROM samples WHERE site_id = '{}' AND parameter_id = '{}' \
                 AND collected_at = '{T1}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_DO_ID
            ),
        ))
        .await
        .unwrap()
        .expect("the sample exists");
    (
        row.try_get::<Option<f64>>("", "mean")
            .unwrap()
            .unwrap_or(f64::NAN),
        row.try_get::<i32>("", "n").unwrap(),
    )
}

async fn withdrawn_index(db: &DatabaseConnection, stream_id: &str, index: i16) -> bool {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT withdrawn_at IS NOT NULL AS w FROM readings \
             WHERE stream_id = '{stream_id}' AND time = '{T1}' AND replicate_index = {index}"
        ),
    ))
    .await
    .unwrap()
    .expect("the reading exists")
    .try_get::<bool>("", "w")
    .unwrap()
}

/// The proposal queue as the review surface reads it.
async fn proposals(fx: &Fixture, status: &str) -> serde_json::Value {
    let (code, body) = crate::common::get_json_with_token(
        &fx.app,
        &format!(
            "/api/reading_change_proposals?filter={}",
            crate::common::e2e::percent_encode(&format!(r#"{{"status":"{status}"}}"#))
        ),
        &fx.token,
    )
    .await;
    assert_eq!(code, 200, "{body}");
    body
}

async fn decide(fx: &Fixture, ids: &[&str], decision: &str) -> (u16, serde_json::Value) {
    let (code, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/sync/change_proposals/decide",
        &serde_json::json!({ "ids": ids, "decision": decision }),
        &fx.token,
    )
    .await;
    (
        code,
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body)),
    )
}

#[tokio::test]
#[serial]
async fn a_windowed_resend_converges_on_the_source() {
    let fx = setup().await;

    // First pass: three replicates, complete content.
    let (status, resp) =
        windowed_ingest(&fx, replicates(T1, &[(0, 10.0), (1, 20.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["inserted"], 3, "{resp}");
    assert_eq!(
        resp["accepted_window"]["source_rows_read"], 1,
        "the claim is echoed: {resp}"
    );
    assert_eq!(sample_stats(&fx.db).await, (22.0, 3));

    // Steady state: the same content re-sent is a recorded no-op.
    let (status, resp) =
        windowed_ingest(&fx, replicates(T1, &[(0, 10.0), (1, 20.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["inserted"], 0, "{resp}");
    assert_eq!(resp["unchanged"], 3, "{resp}");
    assert_eq!(resp["withdrawn"], 0, "{resp}");

    // The source removed replicate 1: the re-send withdraws it, a stamp, and the statistics
    // follow.
    let (status, resp) = windowed_ingest(&fx, replicates(T1, &[(0, 10.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["withdrawn"], 1, "{resp}");
    assert!(withdrawn_index(&fx.db, &fx.stream_id, 1).await);
    assert_eq!(
        sample_stats(&fx.db).await,
        (23.0, 2),
        "served statistics exclude the retraction"
    );

    // The source corrected replicate 0. The change is classified and proposed, not written: the
    // stored value and the statistics stand until a person accepts it (Q84).
    let (status, resp) = windowed_ingest(&fx, replicates(T1, &[(0, 12.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["changed"], 1, "{resp}");
    assert_eq!(
        resp["proposed"], 1,
        "the response says how many of the changes wait for a person: {resp}"
    );
    assert_eq!(
        sample_stats(&fx.db).await,
        (23.0, 2),
        "a proposed correction has not moved the served value"
    );

    // Re-asserting the same number every cycle re-proposes nothing.
    let (_, _) = windowed_ingest(&fx, replicates(T1, &[(0, 12.0), (2, 36.0)]), 1).await;
    let pending = proposals(&fx, "pending").await;
    assert_eq!(
        pending.as_array().map(Vec::len),
        Some(1),
        "one proposal, however many times the source asserts it: {pending}"
    );

    // Accepted, the correction is written, and the statistics follow.
    let (status, resp) = decide(&fx, &[pending[0]["id"].as_str().unwrap()], "accept").await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["accepted"], 1, "{resp}");
    assert_eq!(sample_stats(&fx.db).await, (24.0, 2));

    // The source restored replicate 1: an honest window re-asserting the row clears the stamp.
    let (status, resp) =
        windowed_ingest(&fx, replicates(T1, &[(0, 12.0), (1, 20.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");
    assert!(
        !withdrawn_index(&fx.db, &fx.stream_id, 1).await,
        "reinstated"
    );
    assert_eq!(sample_stats(&fx.db).await, ((12.0 + 20.0 + 36.0) / 3.0, 3));

    // Every pass left a receipt whose arithmetic the database CHECKed on commit.
    let receipts = crate::common::e2e::count(
        &fx.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM ingest_receipts WHERE stream_id = '{}'",
            fx.stream_id
        ),
    )
    .await;
    assert_eq!(receipts, 6);
}

#[tokio::test]
#[serial]
async fn a_flagged_reading_is_held_not_withdrawn() {
    let fx = setup().await;
    let (status, resp) =
        windowed_ingest(&fx, replicates(T1, &[(0, 10.0), (1, 20.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");

    crate::common::exec(
        &fx.db,
        &format!(
            "UPDATE readings SET is_flagged = TRUE, flag_reason = 'outlier under review' \
             WHERE stream_id = '{}' AND time = '{T1}' AND replicate_index = 0",
            fx.stream_id
        ),
    )
    .await;

    // The source no longer holds replicate 0. An operator ruled on that reading, so it stays
    // exactly as they left it and the disagreement lands in the review queue.
    let (status, resp) = windowed_ingest(&fx, replicates(T1, &[(1, 20.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["withdrawn"], 0, "{resp}");
    assert!(
        !withdrawn_index(&fx.db, &fx.stream_id, 0).await,
        "never stamped"
    );

    let hold = fx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT kind, status FROM replicate_audit_holds \
                 WHERE stream_id = '{}' AND group_time = '{T1}'",
                fx.stream_id
            ),
        ))
        .await
        .unwrap()
        .expect("the disagreement is a review item");
    assert_eq!(
        hold.try_get::<String>("", "kind").unwrap(),
        "source_modified"
    );
    assert_eq!(hold.try_get::<String>("", "status").unwrap(), "pending");
}

/// The boundary, end to end: sync corrects the measurement, the operator's ruling stands
/// untouched beside it, and the hold names the ruling the re-send collided with.
#[tokio::test]
#[serial]
async fn a_correction_on_a_judged_reading_waits_for_the_person_who_judged_it() {
    let fx = setup().await;
    let (status, resp) =
        windowed_ingest(&fx, replicates(T1, &[(0, 10.0), (1, 20.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");

    let (status, resp) = crate::common::patch_json_with_token(
        &fx.app,
        "/api/readings/flag",
        &serde_json::json!({
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
                "time": T1,
                "replicate_index": 0
            }],
            "reason": "outlier under review"
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "{resp}");

    // The source corrects the value the operator flagged. Nothing is written: the correction is a
    // proposal like any other, and the operator's ruling is not overwritten behind their back.
    let (status, resp) =
        windowed_ingest(&fx, replicates(T1, &[(0, 11.5), (1, 20.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");

    let stored = |db: &DatabaseConnection, stream_id: String| {
        let q = format!(
            "SELECT raw_value, is_flagged FROM readings \
             WHERE stream_id = '{stream_id}' AND time = '{T1}' AND replicate_index = 0"
        );
        let db = db.clone();
        async move {
            let row = db
                .query_one_raw(Statement::from_string(DatabaseBackend::Postgres, q))
                .await
                .unwrap()
                .expect("the reading");
            (
                row.try_get::<f64>("", "raw_value").unwrap(),
                row.try_get::<Option<bool>>("", "is_flagged").unwrap(),
            )
        }
    };
    assert_eq!(
        stored(&fx.db, fx.stream_id.clone()).await,
        (10.0, Some(true)),
        "the stored value stands until the correction is accepted"
    );

    let pending = proposals(&fx, "pending").await;
    assert_eq!(pending.as_array().map(Vec::len), Some(1), "{pending}");
    assert!(
        (pending[0]["proposed_raw_value"].as_f64().unwrap() - 11.5).abs() < 1e-9,
        "the proposal carries the source's number: {pending}"
    );
    assert!(
        (pending[0]["stored_raw_value"].as_f64().unwrap() - 10.0).abs() < 1e-9,
        "and the one it would replace: {pending}"
    );

    // Rejected, the number stays where it is and the source re-asserting it proposes nothing new.
    let id = pending[0]["id"].as_str().unwrap().to_string();
    let (status, resp) = decide(&fx, &[&id], "reject").await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["rejected"], 1, "{resp}");
    let (_, _) = windowed_ingest(&fx, replicates(T1, &[(0, 11.5), (1, 20.0), (2, 36.0)]), 1).await;
    assert_eq!(
        proposals(&fx, "pending").await.as_array().map(Vec::len),
        Some(0),
        "a decision taken on this exact number is not asked again"
    );

    // The same proposal is still listed, and accepting it later applies it: the ruling was on the
    // value, not on the queue.
    let (status, resp) = decide(&fx, &[&id], "accept").await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        stored(&fx.db, fx.stream_id.clone()).await,
        (11.5, Some(true)),
        "accepted, the value moves and the operator's flag stands"
    );
    let corrections = crate::common::e2e::count(
        &fx.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reading_decisions \
             WHERE stream_id = '{}' AND time = '{T1}' AND replicate_index = 0 \
               AND kind = 'value_correction' AND origin = 'sync'",
            fx.stream_id
        ),
    )
    .await;
    assert_eq!(
        corrections, 1,
        "the accepted correction is on the curation record"
    );
}

#[tokio::test]
#[serial]
async fn dishonest_windows_are_refused_and_windows_are_sync_only() {
    let fx = setup().await;
    let (status, resp) = windowed_ingest(&fx, replicates(T1, &[(0, 10.0), (1, 20.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");

    // An empty payload claiming source rows over stored content: refused, applies nothing.
    let (status, resp) = windowed_ingest(&fx, vec![], 5).await;
    assert_eq!(status, 400, "{resp}");
    assert!(
        resp.to_string().contains("never read as a deletion"),
        "{resp}"
    );
    assert!(!withdrawn_index(&fx.db, &fx.stream_id, 0).await);

    // A claim of zero source rows over stored content: equally refused.
    let (status, resp) = windowed_ingest(&fx, replicates(T1, &[(0, 10.0)]), 0).await;
    assert_eq!(status, 400, "{resp}");

    // A window from a non-sync caller is refused: the claim belongs to the replication layer.
    let (status, resp) = crate::common::post_json_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": fx.stream_id,
            "window": window(1),
            "readings": replicates(T1, &[(0, 10.0)]),
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 403, "{resp}");

    // A window on a continuous stream is refused: withdrawal is spot-only by CHECK.
    let (status, stream) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "stn:cont", "measurement_type": "continuous"}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "{stream}");
    let continuous = crate::common::e2e::id_of(&stream);
    let (status, resp) = crate::common::post_json_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": continuous,
            "window": window(1),
            "readings": [{ "time": T1, "raw_value": 1.0 }],
        }),
        &fx.sync_token,
    )
    .await;
    assert_eq!(status, 400, "{resp}");
    assert!(resp.contains("append-only"), "{resp}");
}

#[tokio::test]
#[serial]
async fn a_bulk_reshape_is_braked_and_new_rows_still_apply() {
    let fx = setup().await;

    // Ten stored instants.
    let mut readings = Vec::new();
    for h in 0..10 {
        readings.push(json!({
            "time": format!("2025-06-01T{h:02}:00:00Z"),
            "raw_value": 10.0 + f64::from(h),
            "replicate_index": 0,
        }));
    }
    let (status, resp) = windowed_ingest(&fx, readings, 10).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["inserted"], 10);

    // A pass claiming only two of them survive (plus one new row): 80% withdrawal trips the
    // brake, corrections and withdrawals hold, the new row still lands.
    let (status, resp) = windowed_ingest_digest(
        &fx,
        vec![
            json!({ "time": "2025-06-01T00:00:00Z", "raw_value": 10.0, "replicate_index": 0 }),
            json!({ "time": "2025-06-01T01:00:00Z", "raw_value": 11.0, "replicate_index": 0 }),
            json!({ "time": "2025-06-01T12:00:00Z", "raw_value": 99.0, "replicate_index": 0 }),
        ],
        3,
        "braked-claim",
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["withdrawn"], 0, "held, not applied: {resp}");
    assert_eq!(resp["inserted"], 1, "the new row applied: {resp}");
    assert_eq!(
        stored_digest(&fx.db, &fx.stream_id).await,
        None,
        "a braked pass claims no digest, so the source keeps re-asserting"
    );

    let intact = crate::common::e2e::count(
        &fx.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE stream_id = '{}' AND withdrawn_at IS NULL",
            fx.stream_id
        ),
    )
    .await;
    assert_eq!(intact, 11, "nothing was withdrawn under the brake");

    let hold = fx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT kind FROM replicate_audit_holds WHERE stream_id = '{}' \
                 AND kind = 'brake_fired'",
                fx.stream_id
            ),
        ))
        .await
        .unwrap();
    assert!(hold.is_some(), "the reshape is a review item");

    let braked = crate::common::e2e::count(
        &fx.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM ingest_receipts \
             WHERE stream_id = '{}' AND braked",
            fx.stream_id
        ),
    )
    .await;
    assert_eq!(braked, 1);

    // The release path: the operator acknowledges the brake hold, ruling the reshape
    // legitimate; the next identical pass applies in full and consumes the ruling.
    let hold_id = fx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT id::text AS id FROM replicate_audit_holds \
                 WHERE stream_id = '{}' AND kind = 'brake_fired'",
                fx.stream_id
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "id")
        .unwrap();
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/acknowledge"),
        &json!({}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "acknowledge the brake: {body}");

    let (status, resp) = windowed_ingest(
        &fx,
        vec![
            json!({ "time": "2025-06-01T00:00:00Z", "raw_value": 10.0, "replicate_index": 0 }),
            json!({ "time": "2025-06-01T01:00:00Z", "raw_value": 11.0, "replicate_index": 0 }),
            json!({ "time": "2025-06-01T12:00:00Z", "raw_value": 99.0, "replicate_index": 0 }),
        ],
        3,
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        resp["withdrawn"], 8,
        "the acknowledged reshape applies: {resp}"
    );
    let intact = crate::common::e2e::count(
        &fx.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE stream_id = '{}' AND withdrawn_at IS NULL",
            fx.stream_id
        ),
    )
    .await;
    assert_eq!(intact, 3, "only the asserted content stays served");

    // The ruling is consumed: the hold is terminal and a fresh reshape would brake anew.
    let remediated = fx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT status FROM replicate_audit_holds WHERE id = '{hold_id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "status")
        .unwrap();
    assert_eq!(remediated, "remediated");
}

#[tokio::test]
#[serial]
async fn a_dropped_replicate_index_is_braked_by_the_index_arm() {
    // Scenario: a source silently stops emitting one member column. Only a minority of the
    // window's total rows withdraw, so the row-fraction brake arm never sees it; the
    // index-fraction arm does, because that one index is wiped entirely.
    //
    // Fixture: 40 instants carry index 0, five of them also carry index 1 (45 stored rows). The
    // re-assert keeps every index-0 value unchanged and drops index 1.
    //   over_floor:          would_touch = 5 withdrawals >= RECONCILE_BRAKE_MIN_ROWS (5)     -> true
    //   over_fraction:       5 / 45 = 0.111 <= RECONCILE_BRAKE_FRACTION (0.15)               -> false
    //   over_index_fraction: index 1 withdrawn 5/5 = 1.0 > RECONCILE_BRAKE_INDEX_FRACTION (0.5) -> true
    // The row-fraction arm cannot fire on this fixture, so a brake here is the index arm alone.
    let fx = setup().await;

    let mut readings = Vec::new();
    for m in 0..40 {
        readings.push(json!({
            "time": format!("2025-06-01T08:{m:02}:00Z"),
            "raw_value": 10.0 + f64::from(m),
            "replicate_index": 0,
        }));
    }
    for m in 0..5 {
        readings.push(json!({
            "time": format!("2025-06-01T08:{m:02}:00Z"),
            "raw_value": 20.0 + f64::from(m),
            "replicate_index": 1,
        }));
    }
    let (status, resp) = windowed_ingest(&fx, readings, 45).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["inserted"], 45, "{resp}");

    let mut reassert = Vec::new();
    for m in 0..40 {
        reassert.push(json!({
            "time": format!("2025-06-01T08:{m:02}:00Z"),
            "raw_value": 10.0 + f64::from(m),
            "replicate_index": 0,
        }));
    }
    let (status, resp) = windowed_ingest(&fx, reassert, 40).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        resp["withdrawn"], 0,
        "braked pass applies only new rows: {resp}"
    );

    let intact = crate::common::e2e::count(
        &fx.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE stream_id = '{}' AND withdrawn_at IS NULL",
            fx.stream_id
        ),
    )
    .await;
    assert_eq!(intact, 45, "the wiped index is held, not withdrawn");
    assert!(
        !withdrawn_index(&fx.db, &fx.stream_id, 1).await,
        "index 1 is held for review, not stamped withdrawn"
    );

    let holds = crate::common::e2e::count(
        &fx.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM replicate_audit_holds \
             WHERE stream_id = '{}' AND kind = 'brake_fired'",
            fx.stream_id
        ),
    )
    .await;
    assert_eq!(
        holds, 1,
        "the dropped index raises exactly one brake review item"
    );
}

#[tokio::test]
#[serial]
async fn the_digest_handshake_stores_only_clean_claims() {
    let fx = setup().await;

    // A clean first pass persists the client's claim.
    let (status, resp) =
        windowed_ingest_digest(&fx, replicates(T1, &[(0, 10.0), (1, 20.0)]), 1, "d1").await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        stored_digest(&fx.db, &fx.stream_id).await,
        Some("d1".to_string())
    );

    let row_xmin = |db: &DatabaseConnection, stream_id: &str| {
        let q = format!(
            "SELECT xmin::text AS x FROM readings \
             WHERE stream_id = '{stream_id}' AND time = '{T1}' AND replicate_index = 0"
        );
        let db = db.clone();
        async move {
            db.query_one_raw(Statement::from_string(DatabaseBackend::Postgres, q))
                .await
                .unwrap()
                .expect("the reading exists")
                .try_get::<String>("", "x")
                .unwrap()
        }
    };
    let before = row_xmin(&fx.db, &fx.stream_id).await;

    // An identical re-send is a recorded no-op: the receipt commits, but no reading row is
    // rewritten (the client would normally not even send this pass).
    let (status, resp) =
        windowed_ingest_digest(&fx, replicates(T1, &[(0, 10.0), (1, 20.0)]), 1, "d1").await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["unchanged"], 2, "{resp}");
    assert_eq!(
        row_xmin(&fx.db, &fx.stream_id).await,
        before,
        "no row version churn"
    );
    let receipts = crate::common::e2e::count(
        &fx.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM ingest_receipts WHERE stream_id = '{}'",
            fx.stream_id
        ),
    )
    .await;
    assert_eq!(receipts, 2, "the ledger still records the pass");

    // A pass that raises a hold stores no digest: the operator's ruling is pending, so the
    // source must keep re-asserting the window.
    crate::common::exec(
        &fx.db,
        &format!(
            "UPDATE readings SET is_flagged = TRUE, flag_reason = 'under review' \
             WHERE stream_id = '{}' AND time = '{T1}' AND replicate_index = 0",
            fx.stream_id
        ),
    )
    .await;
    let (status, resp) = windowed_ingest_digest(&fx, replicates(T1, &[(1, 20.0)]), 1, "d2").await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        stored_digest(&fx.db, &fx.stream_id).await,
        Some("d1".to_string()),
        "the held pass did not update the claim"
    );
}

async fn hold_status_at(db: &DatabaseConnection, stream_id: &str, time: &str) -> Option<String> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT status FROM replicate_audit_holds WHERE stream_id = '{stream_id}' \
             AND kind = 'replicate_stats' AND group_time = '{time}' ORDER BY created_at DESC LIMIT 1"
        ),
    ))
    .await
    .unwrap()
    .map(|r| r.try_get::<String>("", "status").unwrap())
}

#[tokio::test]
#[serial]
async fn a_braked_pass_leaves_its_holds_where_they_were() {
    // Scenario: an instant holds a pending statistics hold. A later pass corrects that instant's
    // replicates to values that agree with the portal's claim, but the pass reshapes the window
    // enough to trip the brake, so the corrections are not applied.
    // Expected behaviour: the stored replicates still disagree with the portal, so the hold that
    // records it stays pending; the audit judges what was stored, not what was offered.
    let fx = setup().await;
    let t0 = "2025-06-01T00:00:00Z";
    let mut readings = replicates(t0, &[(0, 10.0), (1, 12.0)]);
    for h in 1..10 {
        readings.push(json!({
            "time": format!("2025-06-01T{h:02}:00:00Z"),
            "raw_value": 10.0 + f64::from(h),
            "replicate_index": 0,
        }));
    }
    let (status, resp) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": fx.stream_id, "collection": true, "window": window(10),
            "readings": readings,
            "audit": [{ "time": t0, "expected_mean": 50.0, "expected_sd": 0.0, "expected_n": 2 }],
        }),
        &fx.sync_token,
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(
        hold_status_at(&fx.db, &fx.stream_id, t0).await.as_deref(),
        Some("pending"),
        "the portal's claim disagrees with what was stored"
    );

    // Corrected to agree with the claim, inside a pass that withdraws eight of ten instants.
    let mut braked = replicates(t0, &[(0, 50.0), (1, 50.0)]);
    braked.push(json!({ "time": "2025-06-01T01:00:00Z", "raw_value": 11.0, "replicate_index": 0 }));
    let (status, resp) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": fx.stream_id, "collection": true, "window": window(3),
            "readings": braked,
            "audit": [{ "time": t0, "expected_mean": 50.0, "expected_sd": 0.0, "expected_n": 2 }],
        }),
        &fx.sync_token,
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["withdrawn"], 0, "the brake held: {resp}");
    let stored: Vec<f64> = fx
        .db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value FROM readings WHERE stream_id = '{}' AND time = '{t0}' \
                 ORDER BY replicate_index",
                fx.stream_id
            ),
        ))
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get::<f64>("", "raw_value").unwrap())
        .collect();
    assert_eq!(stored, vec![10.0, 12.0], "the correction was not applied");
    assert_eq!(
        hold_status_at(&fx.db, &fx.stream_id, t0).await.as_deref(),
        Some("pending"),
        "the hold records a disagreement that is still stored, so it stays"
    );
}

/// Scenario: a reconciled pass whose only effect is to retract an instant, on a site that has an
/// active derived parameter.
///
/// Expected behaviour: the recompute the pass enqueues covers the retracted instant. A withdrawn
/// key is absent from the payload by construction, so a timestamp list built from the payload
/// alone leaves the derived output standing on an input the source has taken back, and the next
/// honest window reports the same absence, so nothing re-enqueues it.
#[tokio::test]
#[serial]
async fn a_withdrawal_enqueues_the_recompute_at_the_retracted_instant() {
    use crate::common::exec;

    let fx = setup().await;
    exec(
        &fx.db,
        "INSERT INTO parameters (id, code, name, default_units, category) \
         VALUES ('00000000-0000-4000-b000-0000000009d0', 'WDiffDerived', 'WDiff derived', 'x', \
                 'measurement')",
    )
    .await;
    exec(
        &fx.db,
        &format!(
            "INSERT INTO site_parameters \
                 (id, site_id, parameter_id, name, sensor_type, is_active, entry_mode) \
             VALUES ('00000000-0000-4000-a000-0000000009d1', '{site}', \
                     '00000000-0000-4000-b000-0000000009d0', 'WDiffDerived', 'WDiffDerived', \
                     true, 'tool')",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;

    const T2: &str = "2025-06-01T09:00:00Z";
    let mut both = replicates(T1, &[(0, 3.0)]);
    both.extend(replicates(T2, &[(0, 4.0)]));
    let (status, body) = windowed_ingest(&fx, both, 2).await;
    assert!((200..300).contains(&status), "first pass: {body}");

    exec(&fx.db, "DELETE FROM reprocessing_jobs").await;

    // The same window without T2: nothing new, nothing changed, one whole instant withdrawn and
    // so absent from the payload the enqueue reads its timestamps from.
    let (status, body) = windowed_ingest(&fx, replicates(T1, &[(0, 3.0)]), 1).await;
    assert!((200..300).contains(&status), "withdrawing pass: {body}");

    let payload = fx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT params::text AS p FROM reprocessing_jobs \
             WHERE trigger_type = 'ingest_derived' ORDER BY created_at DESC LIMIT 1"
                .to_string(),
        ))
        .await
        .expect("query")
        .map(|row| row.try_get::<String>("", "p").expect("payload"))
        .expect("a withdrawal is an effect, so a recompute is enqueued");

    assert!(
        payload.contains("2025-06-01T09:00:00"),
        "the retracted instant is in the recompute's timestamps: {payload}"
    );
}
