//! S8, windowed reconciliation (PLAN.md story catalog).
//!
//! Scenario: a sync service re-reads its mutable source in full and asserts a completeness
//! window. The store converges — a removed replicate is withdrawn (a stamp, never a delete), a
//! corrected value is applied in place, an unchanged re-send is a recorded no-op — and a reading
//! an operator has flagged never changes servedness without a person: the withdrawal is held in
//! the review queue instead. Dishonest windows are refused outright, and a pass reshaping the
//! window at scale is braked.

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
    db.query_one(Statement::from_string(
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
        .query_one(Statement::from_string(
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
        row.try_get::<Option<f64>>("", "mean").unwrap().unwrap_or(f64::NAN),
        row.try_get::<i32>("", "n").unwrap(),
    )
}

async fn withdrawn_index(db: &DatabaseConnection, stream_id: &str, index: i16) -> bool {
    db.query_one(Statement::from_string(
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

#[tokio::test]
#[serial]
async fn a_windowed_resend_converges_on_the_source() {
    let fx = setup().await;

    // First pass: three replicates, complete content.
    let (status, resp) =
        windowed_ingest(&fx, replicates(T1, &[(0, 10.0), (1, 20.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["inserted"], 3, "{resp}");
    assert_eq!(resp["accepted_window"]["source_rows_read"], 1, "the claim is echoed: {resp}");
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
    assert_eq!(sample_stats(&fx.db).await, (23.0, 2), "served statistics exclude the retraction");

    // The source corrected replicate 0: applied in place, flags and sample links untouched.
    let (status, resp) = windowed_ingest(&fx, replicates(T1, &[(0, 12.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["changed"], 1, "{resp}");
    assert_eq!(sample_stats(&fx.db).await, (24.0, 2));

    // The source restored replicate 1: an honest window re-asserting the row clears the stamp.
    let (status, resp) =
        windowed_ingest(&fx, replicates(T1, &[(0, 12.0), (1, 20.0), (2, 36.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");
    assert!(!withdrawn_index(&fx.db, &fx.stream_id, 1).await, "reinstated");
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
    assert_eq!(receipts, 5);
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
    assert!(!withdrawn_index(&fx.db, &fx.stream_id, 0).await, "never stamped");

    let hold = fx
        .db
        .query_one(Statement::from_string(
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
    assert_eq!(hold.try_get::<String>("", "kind").unwrap(), "source_modified");
    assert_eq!(hold.try_get::<String>("", "status").unwrap(), "pending");
}

#[tokio::test]
#[serial]
async fn dishonest_windows_are_refused_and_windows_are_sync_only() {
    let fx = setup().await;
    let (status, resp) =
        windowed_ingest(&fx, replicates(T1, &[(0, 10.0), (1, 20.0)]), 1).await;
    assert_eq!(status, 200, "{resp}");

    // An empty payload claiming source rows over stored content: refused, applies nothing.
    let (status, resp) = windowed_ingest(&fx, vec![], 5).await;
    assert_eq!(status, 400, "{resp}");
    assert!(resp.to_string().contains("never read as a deletion"), "{resp}");
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
        .query_one(Statement::from_string(
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
        .query_one(Statement::from_string(
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
    assert_eq!(resp["withdrawn"], 8, "the acknowledged reshape applies: {resp}");
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
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT status FROM replicate_audit_holds WHERE id = '{hold_id}'"
            ),
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
async fn the_digest_handshake_stores_only_clean_claims() {
    let fx = setup().await;

    // A clean first pass persists the client's claim.
    let (status, resp) =
        windowed_ingest_digest(&fx, replicates(T1, &[(0, 10.0), (1, 20.0)]), 1, "d1").await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(stored_digest(&fx.db, &fx.stream_id).await, Some("d1".to_string()));

    let row_xmin = |db: &DatabaseConnection, stream_id: &str| {
        let q = format!(
            "SELECT xmin::text AS x FROM readings \
             WHERE stream_id = '{stream_id}' AND time = '{T1}' AND replicate_index = 0"
        );
        let db = db.clone();
        async move {
            db.query_one(Statement::from_string(DatabaseBackend::Postgres, q))
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
    assert_eq!(row_xmin(&fx.db, &fx.stream_id).await, before, "no row version churn");
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
