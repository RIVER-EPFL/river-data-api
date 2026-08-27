//! The sync-time replicate audit: `/ingest` recomputes each audited group's mean/sd over the
//! values it stores and always admits the group; a disagreement records a hold for review
//! (`pending` on a paired stream, `deferred` until pairing). Resolutions never write a
//! statistic: the operator accepts the recomputed numbers or flags replicates so the sample
//! recomputes, both recorded on the hold and reversible via reopen.
//!
//! Run: cargo test --test sync replicate_audit -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-06-01T08:00:00Z";
const T2: &str = "2025-06-01T09:00:00Z";

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
    sync_token: String,
    stream: String,
}

async fn setup_unpaired(key: &str) -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "auditsrc", "source_key": key, "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "register: {stream}");
    let stream = crate::common::e2e::id_of(&stream);

    Fixture {
        db,
        app,
        token,
        sync_token,
        stream,
    }
}

async fn pair(fx: &Fixture) {
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        &format!("/api/streams/{}/pair", fx.stream),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");
}

async fn setup(key: &str) -> Fixture {
    let fx = setup_unpaired(key).await;
    pair(&fx).await;
    fx
}

fn group(time: &str, values: &[f64]) -> Vec<serde_json::Value> {
    values
        .iter()
        .enumerate()
        .map(|(i, v)| json!({"time": time, "raw_value": v, "replicate_index": i}))
        .collect()
}

fn sparse_group(time: &str, members: &[(i16, f64)]) -> Vec<serde_json::Value> {
    members
        .iter()
        .map(|(i, v)| json!({"time": time, "raw_value": v, "replicate_index": i}))
        .collect()
}

async fn ingest_audited(
    fx: &Fixture,
    readings: Vec<serde_json::Value>,
    audit: serde_json::Value,
) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({"stream_id": fx.stream, "readings": readings, "audit": audit}),
        &fx.sync_token,
    )
    .await;
    assert_eq!(status, 200, "audited ingest ({status}): {body}");
    body
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    crate::common::e2e::count(db, sql).await
}

async fn readings_at(fx: &Fixture, time: &str) -> i64 {
    count(
        &fx.db,
        &format!(
            "SELECT COUNT(*) FROM readings WHERE stream_id = '{}' AND time = '{time}'",
            fx.stream
        ),
    )
    .await
}

async fn flagged_at(fx: &Fixture, time: &str) -> i64 {
    count(
        &fx.db,
        &format!(
            "SELECT COUNT(*) FROM readings WHERE stream_id = '{}' AND time = '{time}' \
             AND is_flagged = TRUE",
            fx.stream
        ),
    )
    .await
}

async fn list_holds(fx: &Fixture, extra: &str) -> serde_json::Value {
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

async fn pending_hold_id(fx: &Fixture) -> String {
    list_holds(fx, "").await["holds"][0]["id"]
        .as_str()
        .unwrap()
        .to_string()
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

async fn sample_stats(fx: &Fixture, time: &str) -> Option<(f64, Option<f64>, i64)> {
    fx.db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT s.mean, s.stdev, s.n::bigint AS n FROM samples s \
                 WHERE s.site_id = '{}' AND s.parameter_id = '{}' AND s.collected_at = '{time}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .map(|row| {
            (
                row.try_get::<f64>("", "mean").unwrap(),
                row.try_get::<Option<f64>>("", "stdev").unwrap(),
                row.try_get::<i64>("", "n").unwrap(),
            )
        })
}

#[tokio::test]
#[serial]
async fn matching_audit_passes() {
    let fx = setup("audit-match").await;

    let body = ingest_audited(
        &fx,
        group(T1, &[10.0, 20.0, 30.0]),
        json!([{"time": T1, "expected_mean": 20.0, "expected_sd": 10.0}]),
    )
    .await;
    assert_eq!(body["inserted"], 3);
    assert_eq!(body["held"], 0);
    assert_eq!(
        count(&fx.db, "SELECT COUNT(*) FROM replicate_audit_holds").await,
        0,
        "agreement leaves no hold behind"
    );
}

#[tokio::test]
#[serial]
async fn mismatch_admits_group_and_records_pending_hold() {
    let fx = setup("audit-mismatch").await;

    let mut readings = group(T1, &[10.0, 20.0, 30.0]);
    readings.extend(group(T2, &[40.0, 50.0, 60.0]));
    let body = ingest_audited(
        &fx,
        readings,
        json!([
            {"time": T1, "expected_mean": 25.0, "expected_sd": 10.0},
            {"time": T2, "expected_mean": 50.0, "expected_sd": 10.0},
        ]),
    )
    .await;
    assert_eq!(body["inserted"], 6, "both groups are admitted: {body}");
    assert_eq!(body["held"], 0);
    assert_eq!(readings_at(&fx, T1).await, 3);

    let holds = list_holds(&fx, "").await;
    assert_eq!(holds["total"], 1);
    assert_eq!(holds["pending"], 1);
    let hold = &holds["holds"][0];
    assert_eq!(hold["status"], "pending");
    assert_eq!(hold["source_system"], "auditsrc");
    assert_eq!(hold["source_key"], "audit-mismatch");
    assert_eq!(hold["expected"]["mean"], 25.0);
    assert_eq!(hold["computed"]["mean"], 20.0);
    assert_eq!(hold["computed"]["n"], 3);
    assert_eq!(hold["delta"]["mean"], 5.0);
    assert!(hold["resolution"].is_null());

    let (mean, _, _) = sample_stats(&fx, T1).await.expect("a sample forms");
    assert!(
        (mean - 20.0).abs() < 1e-9,
        "the recomputed mean serves immediately: {mean}"
    );

    assert_eq!(
        count(
            &fx.db,
            &format!(
                "SELECT COUNT(*) FROM data_streams WHERE id = '{}' \
                 AND last_data_time = '{T2}'",
                fx.stream
            ),
        )
        .await,
        1,
        "the cursor advances past the disagreeing group"
    );
}

#[tokio::test]
#[serial]
async fn acknowledged_decision_stands_against_redetection() {
    let fx = setup("audit-ack").await;

    let batch = group(T1, &[10.0, 20.0, 30.0]);
    let audit = json!([{"time": T1, "expected_mean": 25.0, "expected_sd": 10.0}]);
    ingest_audited(&fx, batch.clone(), audit.clone()).await;

    let hold_id = pending_hold_id(&fx).await;
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/acknowledge"),
        &json!({}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "acknowledge ({status}): {body}");
    assert_eq!(body["acknowledged"], 1);
    assert_eq!(hold_status(&fx.db, &hold_id).await, "acknowledged");

    let resolved = list_holds(&fx, "&status=resolved").await;
    assert_eq!(resolved["holds"][0]["resolution"]["action"], "accept_ours");
    assert!(!resolved["holds"][0]["acknowledged_by"].is_null());

    let body = ingest_audited(&fx, batch, audit).await;
    assert_eq!(
        body["inserted"], 0,
        "a re-send is duplicate-skipped: {body}"
    );
    assert_eq!(
        count(
            &fx.db,
            &format!(
                "SELECT COUNT(*) FROM replicate_audit_holds WHERE stream_id = '{}'",
                fx.stream
            ),
        )
        .await,
        1,
        "re-detecting the same disagreement does not reopen the decided hold"
    );
    assert_eq!(hold_status(&fx.db, &hold_id).await, "acknowledged");
}

#[tokio::test]
#[serial]
async fn matching_resend_supersedes_stale_hold() {
    let fx = setup("audit-supersede").await;

    let batch = group(T1, &[10.0, 20.0, 30.0]);
    ingest_audited(
        &fx,
        batch.clone(),
        json!([{"time": T1, "expected_mean": 25.0, "expected_sd": 10.0}]),
    )
    .await;
    let hold_id = pending_hold_id(&fx).await;

    let body = ingest_audited(
        &fx,
        batch,
        json!([{"time": T1, "expected_mean": 20.0, "expected_sd": 10.0}]),
    )
    .await;
    assert_eq!(body["held"], 0);
    assert_eq!(hold_status(&fx.db, &hold_id).await, "superseded");
    assert_eq!(readings_at(&fx, T1).await, 3);
}

#[tokio::test]
#[serial]
async fn hold_list_and_bulk_acknowledge() {
    let fx = setup("audit-bulk").await;

    let mut readings = group(T1, &[10.0, 20.0, 30.0]);
    readings.extend(group(T2, &[40.0, 50.0, 60.0]));
    ingest_audited(
        &fx,
        readings,
        json!([
            {"time": T1, "expected_mean": 99.0},
            {"time": T2, "expected_mean": 99.0},
        ]),
    )
    .await;

    let holds = list_holds(&fx, "").await;
    assert_eq!(holds["total"], 2);
    assert_eq!(holds["pending"], 2);
    for hold in holds["holds"].as_array().unwrap() {
        for field in [
            "id",
            "stream_id",
            "source_system",
            "source_key",
            "group_time",
            "expected",
            "computed",
            "delta",
            "status",
            "classification",
            "created_at",
        ] {
            assert!(!hold[field].is_null(), "hold row carries {field}: {hold}");
        }
        assert!(hold["acknowledged_by"].is_null());
        assert!(hold["resolution"].is_null());
    }

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/sync/replicate_audit_holds/acknowledge_bulk",
        &json!({"stream_id": fx.stream, "start": T1, "end": T1}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "bulk acknowledge ({status}): {body}");
    assert_eq!(
        body["acknowledged"], 1,
        "the window covers one hold: {body}"
    );

    let pending = list_holds(&fx, "&status=pending").await;
    assert_eq!(
        pending["total"], 1,
        "the hold outside the window stays pending"
    );
    assert_eq!(
        pending["holds"][0]["group_time"].as_str().unwrap(),
        "2025-06-01T09:00:00Z",
        "the remaining pending hold is the one at T2"
    );
    let resolved = list_holds(&fx, "&status=resolved").await;
    assert_eq!(resolved["holds"][0]["resolution"]["action"], "accept_ours");
}

/// Scenario: one group disagrees by a hair (a systematic small offset), another wildly (a stale
/// aggregate). Expected behaviour: bulk acknowledge with a `max_relative_delta` ceiling accepts
/// only the small one; the large disagreement stays pending for review, and the ceiling uses the
/// same relative_delta the list endpoint reports.
#[tokio::test]
#[serial]
async fn threshold_bulk_acknowledge_takes_only_small_deltas() {
    let fx = setup("audit-threshold").await;

    let mut readings = group(T1, &[10.0, 20.0, 30.0]);
    readings.extend(group(T2, &[40.0, 50.0, 60.0]));
    ingest_audited(
        &fx,
        readings,
        json!([
            // Computed mean 20; off by 0.1 -> relative_delta 0.005.
            {"time": T1, "expected_mean": 20.1},
            // Computed mean 50; off by 25 -> relative_delta 0.5.
            {"time": T2, "expected_mean": 75.0},
        ]),
    )
    .await;

    let holds = list_holds(&fx, "&status=pending").await;
    for hold in holds["holds"].as_array().unwrap() {
        let rel = hold["relative_delta"].as_f64().unwrap();
        let mean_rel = hold["mean_relative_delta"].as_f64().unwrap();
        let sd_rel = hold["sd_relative_delta"].as_f64().unwrap();
        assert!(
            sd_rel.abs() < 1e-12,
            "no sd was audited, so its delta is zero: {sd_rel}"
        );
        assert!(
            (rel - mean_rel).abs() < 1e-12,
            "the overall is the greater of the two: {rel} vs {mean_rel}"
        );
        match hold["group_time"].as_str().unwrap() {
            "2025-06-01T08:00:00Z" => assert!((rel - 0.1 / 20.1).abs() < 1e-6, "T1 rel: {rel}"),
            "2025-06-01T09:00:00Z" => assert!((rel - 1.0 / 3.0).abs() < 1e-6, "T2 rel: {rel}"),
            other => panic!("unexpected hold at {other}"),
        }
    }

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/sync/replicate_audit_holds/acknowledge_bulk",
        &json!({"stream_id": fx.stream, "max_relative_delta": 0.01}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "threshold bulk acknowledge ({status}): {body}");
    assert_eq!(body["acknowledged"], 1, "only the small delta: {body}");

    let pending = list_holds(&fx, "&status=pending").await;
    assert_eq!(pending["total"], 1);
    assert_eq!(
        pending["holds"][0]["group_time"].as_str().unwrap(),
        "2025-06-01T09:00:00Z",
        "the large disagreement stays pending"
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/sync/replicate_audit_holds/acknowledge_bulk",
        &json!({"source_system": "auditsrc", "max_relative_delta": 1.0}),
        &fx.token,
    )
    .await;
    assert_eq!(
        status, 200,
        "source-wide bulk acknowledge ({status}): {body}"
    );
    assert_eq!(
        body["acknowledged"], 1,
        "the large one under a high ceiling: {body}"
    );
}

/// The portals round aggregate cells to 2 decimals before storing, so an expected mean that is
/// the true mean rounded to 2dp must not raise a hold.
#[tokio::test]
#[serial]
async fn quantization_2dp_rounding_is_not_a_mismatch() {
    let fx = setup("audit-quantum").await;

    // True mean 147.3333..., sd 0.28868; the portal's cells store 147.33 and 0.29.
    let body = ingest_audited(
        &fx,
        group(T1, &[147.0, 147.5, 147.5]),
        json!([{"time": T1, "expected_mean": 147.33, "expected_sd": 0.29}]),
    )
    .await;
    assert_eq!(body["inserted"], 3, "{body}");
    assert_eq!(
        count(&fx.db, "SELECT COUNT(*) FROM replicate_audit_holds").await,
        0
    );
}

#[tokio::test]
#[serial]
async fn n_mismatch_holds_even_when_stats_agree() {
    let fx = setup("audit-ncheck").await;

    let body = ingest_audited(
        &fx,
        group(T1, &[10.0, 30.0]),
        json!([{
            "time": T1,
            "expected_mean": 20.0,
            "expected_sd": 14.142_135_623_730_951,
            "expected_n": 3,
        }]),
    )
    .await;
    assert_eq!(body["inserted"], 2, "the group is admitted: {body}");
    assert_eq!(readings_at(&fx, T1).await, 2);

    let holds = list_holds(&fx, "").await;
    assert_eq!(holds["pending"], 1);
    let hold = &holds["holds"][0];
    assert_eq!(hold["expected"]["n"], 3);
    assert_eq!(hold["computed"]["n"], 2);
    assert_eq!(hold["delta"]["n"], 1);
    assert_eq!(hold["classification"], "n_mismatch");
    let values = hold["computed"]["values"].as_array().unwrap();
    assert_eq!(
        values.len(),
        2,
        "the stored values travel with the hold: {hold}"
    );
    assert_eq!(values[0]["index"], 0);
    assert_eq!(values[1]["index"], 1);
}

/// Scenario: a source omits a NULL column cell, so the family arrives at indexes 0, 1 and 3.
/// Expected behaviour: the hold carries each value's own index, a flag applies at that index, and
/// an index the group does not hold flags nothing rather than half-applying.
#[tokio::test]
#[serial]
async fn a_group_with_a_gap_is_flagged_by_its_own_indexes() {
    let fx = setup("audit-gap").await;
    let audit = json!([{"time": T1, "expected_mean": 15.0,
                        "expected_sd": 7.071_067_811_865_476, "expected_n": 3}]);
    ingest_audited(
        &fx,
        sparse_group(T1, &[(0, 10.0), (1, 20.0), (3, 999.0)]),
        audit,
    )
    .await;

    let hold_id = pending_hold_id(&fx).await;
    let holds = list_holds(&fx, "").await;
    let values = holds["holds"][0]["computed"]["values"].as_array().unwrap();
    assert_eq!(values[2]["index"], 3);
    assert_eq!(values[2]["value"], 999.0);

    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "flag", "replicate_indexes": [2, 3]}),
    )
    .await;
    assert_eq!(status, 400, "an absent index flags nothing: {body}");
    assert_eq!(flagged_at(&fx, T1).await, 0);
    assert_eq!(hold_status(&fx.db, &hold_id).await, "pending");

    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "flag", "replicate_indexes": [3]}),
    )
    .await;
    assert_eq!(status, 200, "resolve flag ({status}): {body}");
    assert_eq!(
        count(
            &fx.db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{}' AND time = '{T1}' \
                 AND is_flagged = TRUE AND replicate_index = 3",
                fx.stream
            ),
        )
        .await,
        1
    );

    let (mean, _, n) = sample_stats(&fx, T1).await.expect("sample");
    assert!(
        (mean - 15.0).abs() < 1e-9,
        "recomputed over the rest: {mean}"
    );
    assert_eq!(n, 2);
}

/// A legacy single-column stream superseded by a replicate family (same source_system, family key
/// `<key>:reps`) stays out of pairing plans: its stale metadata carries the retired label
/// identity and would seed duplicate parameter rows.
#[tokio::test]
#[serial]
async fn plan_excludes_family_superseded_legacy() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    for key in ["STA:X_avg", "STA:X_avg:reps", "STA:Y_avg"] {
        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/streams/register",
            &json!({"source_system": "famplan", "source_key": key, "measurement_type": "spot"}),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "register {key}: {body}");
    }

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &json!({"source_system": "famplan"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan ({status}): {plan}");
    let keys: Vec<&str> = plan["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["source_key"].as_str().unwrap())
        .collect();
    assert!(
        !keys.contains(&"STA:X_avg"),
        "the superseded legacy single is excluded: {keys:?}"
    );
    assert!(keys.contains(&"STA:X_avg:reps"), "{keys:?}");
    assert!(
        keys.contains(&"STA:Y_avg"),
        "a single with no family is planned: {keys:?}"
    );
}

#[tokio::test]
#[serial]
async fn unpaired_mismatch_defers_without_gating() {
    let fx = setup_unpaired("audit-deferred").await;

    let batch = group(T1, &[10.0, 20.0, 30.0]);
    let audit = json!([{"time": T1, "expected_mean": 25.0, "expected_sd": 10.0}]);
    let body = ingest_audited(&fx, batch, audit).await;
    assert_eq!(body["inserted"], 3, "unpaired groups are admitted: {body}");
    assert_eq!(readings_at(&fx, T1).await, 3);

    let cursor_at_t1 = count(
        &fx.db,
        &format!(
            "SELECT COUNT(*) FROM data_streams WHERE id = '{}' AND last_data_time = '{T1}'",
            fx.stream
        ),
    )
    .await;
    assert_eq!(cursor_at_t1, 1, "the cursor advances past a deferred group");

    let review = list_holds(&fx, "").await;
    assert_eq!(review["total"], 0, "deferred holds stay out of review");
    assert_eq!(review["deferred"], 1);
    let deferred = list_holds(&fx, "&status=deferred").await;
    assert_eq!(deferred["total"], 1);
    assert_eq!(deferred["holds"][0]["status"], "deferred");
    assert_eq!(deferred["holds"][0]["paired"], false);
}

#[tokio::test]
#[serial]
async fn pairing_promotes_deferred_and_unpairing_defers() {
    let fx = setup_unpaired("audit-promote").await;

    let audit = json!([{"time": T1, "expected_mean": 25.0, "expected_sd": 10.0}]);
    ingest_audited(&fx, group(T1, &[10.0, 20.0, 30.0]), audit).await;
    pair(&fx).await;

    let review = list_holds(&fx, "").await;
    assert_eq!(review["pending"], 1, "pairing promotes the hold: {review}");
    assert_eq!(review["holds"][0]["status"], "pending");
    assert_eq!(review["holds"][0]["paired"], true);
    assert_eq!(review["holds"][0]["site_name"], "Upstream Station");

    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        &format!("/api/streams/{}/unpair", fx.stream),
        &json!({}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "unpair ({status}): {body}");
    let deferred = list_holds(&fx, "&status=deferred").await;
    assert_eq!(deferred["total"], 1, "unpairing defers the open review");
}

#[tokio::test]
#[serial]
async fn resolve_ours_accepts_recomputed_statistics() {
    let fx = setup("audit-ours").await;
    let audit = json!([{"time": T1, "expected_mean": 25.0, "expected_sd": 10.0}]);
    ingest_audited(&fx, group(T1, &[10.0, 20.0, 30.0]), audit).await;

    let hold_id = pending_hold_id(&fx).await;
    let (status, body) = resolve(&fx, &hold_id, &json!({"mode": "ours"})).await;
    assert_eq!(status, 200, "resolve ({status}): {body}");
    assert_eq!(body["status"], "acknowledged");
    assert_eq!(hold_status(&fx.db, &hold_id).await, "acknowledged");
    assert_eq!(
        readings_at(&fx, T1).await,
        3,
        "the replicates stay as stored"
    );
    let (mean, _, n) = sample_stats(&fx, T1).await.expect("sample");
    assert!((mean - 20.0).abs() < 1e-9);
    assert_eq!(n, 3);
}

/// Scenario: the portal's stored aggregate was frozen over the first two replicates before a
/// third, wild value was entered (the stale_subset signature seen in real cnet DOC rows).
/// Expected behaviour: flagging the third replicate reproduces the portal's statistics from the
/// remaining raw data; nothing is overwritten and the decision is recorded and reversible.
#[tokio::test]
#[serial]
async fn resolve_flag_recomputes_sample_and_reopen_reverts() {
    let fx = setup("audit-flag").await;
    // Values 10, 20, 999; the portal's cells were computed over the first two.
    let audit = json!([{"time": T1, "expected_mean": 15.0,
                        "expected_sd": 7.071_067_811_865_476, "expected_n": 3}]);
    ingest_audited(&fx, group(T1, &[10.0, 20.0, 999.0]), audit).await;

    let holds = list_holds(&fx, "").await;
    let hold = &holds["holds"][0];
    let hold_id = hold["id"].as_str().unwrap().to_string();
    assert_eq!(hold["classification"], "stale_subset");

    let (status, body) = resolve(&fx, &hold_id, &json!({"mode": "flag"})).await;
    assert_eq!(status, 400, "flag without indexes is refused: {body}");

    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "flag", "replicate_indexes": [2], "reason": "entry error"}),
    )
    .await;
    assert_eq!(status, 200, "resolve flag ({status}): {body}");
    assert_eq!(body["status"], "remediated");
    assert_eq!(hold_status(&fx.db, &hold_id).await, "remediated");
    assert_eq!(flagged_at(&fx, T1).await, 1);
    assert_eq!(readings_at(&fx, T1).await, 3, "no reading is deleted");

    let (mean, stdev, n) = sample_stats(&fx, T1).await.expect("sample");
    assert!(
        (mean - 15.0).abs() < 1e-9,
        "the sample recomputed over the unflagged replicates: {mean}"
    );
    assert!((stdev.unwrap() - 7.071_067_811_865_476).abs() < 1e-9);
    assert_eq!(n, 2);

    let resolved = list_holds(&fx, "&status=remediated").await;
    let record = &resolved["holds"][0]["resolution"];
    assert_eq!(record["action"], "flag_replicates");
    assert_eq!(record["replicate_indexes"][0], 2);
    assert_eq!(record["reason"], "entry error");

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/reopen"),
        &json!({}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "reopen ({status}): {body}");
    assert_eq!(body["status"], "pending");
    assert_eq!(
        flagged_at(&fx, T1).await,
        0,
        "the resolution's flags revert"
    );
    let (mean, _, n) = sample_stats(&fx, T1).await.expect("sample");
    assert!(
        (mean - 343.0).abs() < 1e-9,
        "the full group serves again: {mean}"
    );
    assert_eq!(n, 3);

    let review = list_holds(&fx, "").await;
    let reopened = &review["holds"][0]["resolution"];
    assert_eq!(reopened["action"], "reopened");
    assert_eq!(
        reopened["history"][0]["action"], "flag_replicates",
        "the decision trail survives the reopen: {reopened}"
    );
}

#[tokio::test]
#[serial]
async fn classification_reads_the_disagreement_signature() {
    let fx = setup("audit-classify").await;
    // Population sd of (10, 20, 30) is 8.165; the sample sd is 10.
    let audit = json!([{"time": T1, "expected_mean": 20.0,
                        "expected_sd": 8.164_965_809_277_26, "expected_n": 3}]);
    ingest_audited(&fx, group(T1, &[10.0, 20.0, 30.0]), audit).await;
    let holds = list_holds(&fx, "").await;
    assert_eq!(holds["holds"][0]["classification"], "population_sd");
}

/// An index that exists in the group but is not among the values the hold recorded cannot be
/// flagged through the hold: the operator's decision is about the values the hold showed.
#[tokio::test]
#[serial]
async fn an_index_outside_the_holds_recorded_values_is_refused() {
    let fx = setup("audit-outside").await;
    let audit = json!([{"time": T1, "expected_mean": 15.0}]);
    ingest_audited(&fx, group(T1, &[10.0, 20.0, 30.0]), audit).await;
    let hold_id = pending_hold_id(&fx).await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": fx.stream,
            "readings": [{"time": T1, "raw_value": 40.0, "replicate_index": 5}],
        }),
        &fx.sync_token,
    )
    .await;
    assert_eq!(status, 200, "late replicate ingests ({status}): {body}");
    assert_eq!(readings_at(&fx, T1).await, 4);

    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "flag", "replicate_indexes": [5]}),
    )
    .await;
    assert_eq!(status, 400, "resolve ({status}): {body}");
    assert!(
        body.to_string().contains("not named by this hold"),
        "the refusal says the hold never showed that index: {body}"
    );
    assert_eq!(flagged_at(&fx, T1).await, 0);
    assert_eq!(hold_status(&fx.db, &hold_id).await, "pending");
}

/// A hold written before indexes travelled with the values holds bare numbers whose indexes are
/// unrecoverable, so the flag mode is refused outright rather than guessed at.
#[tokio::test]
#[serial]
async fn a_legacy_bare_array_hold_refuses_the_flag_mode() {
    let fx = setup("audit-legacy").await;
    let audit = json!([{"time": T1, "expected_mean": 15.0}]);
    ingest_audited(&fx, group(T1, &[10.0, 20.0, 999.0]), audit).await;
    let hold_id = pending_hold_id(&fx).await;
    fx.db
        .execute(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "UPDATE replicate_audit_holds \
                 SET computed = jsonb_set(computed, '{{values}}', '[10.0, 20.0, 999.0]') \
                 WHERE id = '{hold_id}'"
            ),
        ))
        .await
        .unwrap();

    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "flag", "replicate_indexes": [2]}),
    )
    .await;
    assert_eq!(status, 400, "resolve ({status}): {body}");
    assert!(
        body.to_string().contains("predates index recording"),
        "the refusal explains the hold's shape and points elsewhere: {body}"
    );
    assert!(
        body.to_string().contains("readings flag endpoints"),
        "{body}"
    );
    assert_eq!(flagged_at(&fx, T1).await, 0);
    assert_eq!(hold_status(&fx.db, &hold_id).await, "pending");
}

/// Flagging every replicate would leave the sample trigger with n = 0 and the instant would
/// vanish from serving.
#[tokio::test]
#[serial]
async fn at_least_one_unflagged_replicate_must_remain() {
    let fx = setup("audit-allflag").await;
    let audit = json!([{"time": T1, "expected_mean": 15.0}]);
    ingest_audited(&fx, group(T1, &[10.0, 20.0, 999.0]), audit).await;
    let hold_id = pending_hold_id(&fx).await;

    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "flag", "replicate_indexes": [0, 1, 2]}),
    )
    .await;
    assert_eq!(status, 400, "resolve ({status}): {body}");
    assert!(
        body.to_string()
            .contains("at least one unflagged replicate must remain"),
        "{body}"
    );
    assert_eq!(flagged_at(&fx, T1).await, 0);
    assert_eq!(hold_status(&fx.db, &hold_id).await, "pending");

    let (mean, _, n) = sample_stats(&fx, T1).await.expect("sample");
    assert!(
        (mean - 343.0).abs() < 1e-9,
        "still serving all three: {mean}"
    );
    assert_eq!(n, 3);
}

/// An index that exists but is already flagged is refused with a message that says so, not one
/// that reads as "that index does not exist".
#[tokio::test]
#[serial]
async fn an_already_flagged_index_is_distinguished_from_an_absent_one() {
    let fx = setup("audit-preflagged").await;
    let audit = json!([{"time": T1, "expected_mean": 15.0}]);
    ingest_audited(&fx, group(T1, &[10.0, 20.0, 999.0]), audit).await;
    let hold_id = pending_hold_id(&fx).await;

    let (status, body) = crate::common::patch_json_with_token(
        &fx.app,
        "/api/readings/flag",
        &json!({
            "reason": "field note",
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                "time": T1,
                "replicate_index": 2,
            }],
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "pre-flag ({status}): {body}");

    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "flag", "replicate_indexes": [2]}),
    )
    .await;
    assert_eq!(status, 400, "resolve ({status}): {body}");
    assert!(
        body.to_string().contains("already flagged"),
        "the operator learns the true state: {body}"
    );
    assert_eq!(hold_status(&fx.db, &hold_id).await, "pending");
}

/// A terminal decision stands against re-detection of the same disagreement, but a cycle whose
/// expected statistics moved is new evidence the decision never covered.
#[tokio::test]
#[serial]
async fn a_changed_expectation_opens_a_fresh_hold_beside_the_terminal_one() {
    let fx = setup("audit-redetect").await;
    let batch = group(T1, &[10.0, 20.0, 30.0]);
    ingest_audited(
        &fx,
        batch.clone(),
        json!([{"time": T1, "expected_mean": 25.0, "expected_sd": 10.0}]),
    )
    .await;
    let hold_id = pending_hold_id(&fx).await;
    let (status, body) = resolve(&fx, &hold_id, &json!({"mode": "ours"})).await;
    assert_eq!(status, 200, "resolve ({status}): {body}");

    let holds_for_stream = || async {
        count(
            &fx.db,
            &format!(
                "SELECT COUNT(*) FROM replicate_audit_holds WHERE stream_id = '{}'",
                fx.stream
            ),
        )
        .await
    };

    ingest_audited(
        &fx,
        batch.clone(),
        json!([{"time": T1, "expected_mean": 25.0, "expected_sd": 10.0}]),
    )
    .await;
    assert_eq!(
        holds_for_stream().await,
        1,
        "the same disagreement does not reopen the decided hold"
    );

    ingest_audited(
        &fx,
        batch,
        json!([{"time": T1, "expected_mean": 27.0, "expected_sd": 10.0}]),
    )
    .await;
    assert_eq!(
        holds_for_stream().await,
        2,
        "a moved expectation is new evidence and opens a fresh hold"
    );
    let review = list_holds(&fx, "").await;
    assert_eq!(review["pending"], 1);
    assert_eq!(review["holds"][0]["expected"]["mean"], 27.0);
    assert_eq!(hold_status(&fx.db, &hold_id).await, "acknowledged");
}

/// The recorded actor comes from the caller's authentication; a caller-supplied name is ignored,
/// and every resolution entry carries who acted and when.
#[tokio::test]
#[serial]
async fn the_acting_identity_comes_from_auth_and_is_recorded_on_the_resolution() {
    let fx = setup("audit-actor").await;
    ingest_audited(
        &fx,
        group(T1, &[10.0, 20.0, 30.0]),
        json!([{"time": T1, "expected_mean": 25.0}]),
    )
    .await;
    let hold_id = pending_hold_id(&fx).await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/acknowledge"),
        &json!({"acknowledged_by": "mallory"}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "acknowledge ({status}): {body}");

    let resolved = list_holds(&fx, "&status=resolved").await;
    let hold = &resolved["holds"][0];
    let by = hold["acknowledged_by"].as_str().unwrap();
    assert!(
        by.starts_with("token:"),
        "the actor is the authenticated caller, not the payload: {by}"
    );
    assert_eq!(hold["resolution"]["by"].as_str().unwrap(), by);
    assert!(
        hold["resolution"]["at"].as_str().is_some(),
        "the entry records when: {hold}"
    );
}

#[tokio::test]
#[serial]
async fn audit_review_requires_manager_capability() {
    let fx = setup("audit-authz").await;
    let read_only = crate::common::seed_token_read_metadata_only(&fx.db).await;
    let (status, _body) =
        crate::common::get_with_token(&fx.app, "/api/sync/replicate_audit_holds", &read_only).await;
    assert_eq!(
        status, 403,
        "read_metadata alone no longer reaches the audit backlog"
    );

    let write_metadata = crate::common::seed_token_write_metadata_only(&fx.db).await;
    let (status, _body) =
        crate::common::get_with_token(&fx.app, "/api/sync/replicate_audit_holds", &write_metadata)
            .await;
    assert_eq!(
        status, 200,
        "a write_metadata token passes the manager gate"
    );
}

/// The reconciliation delete removes streams and readings, so it takes the stream-deletion gate
/// (administrator or write_metadata token), not the manager review layer: a manager who can
/// resolve holds and start the non-destructive migration cannot start the delete.
#[tokio::test]
#[serial]
async fn the_destructive_reconciliation_delete_refuses_a_manager() {
    if !crate::common::keycloak::keycloak_reachable().await {
        eprintln!("SKIP: keycloak unreachable (start the dev stack, or set TEST_KEYCLOAK_URL)");
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::keycloak::build_test_app_with_keycloak(db.clone()).await;

    let sub = crate::common::keycloak::keycloak_user_id("manager1").await;
    crate::common::keycloak::grant_project(&db, &sub, crate::common::PROJECT_ID).await;
    let manager = crate::common::keycloak::get_keycloak_jwt("manager1", "manager1").await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sync/replicate_reconciliation/delete",
        &json!({"source_system": "cnet"}),
        &manager,
    )
    .await;
    assert_eq!(status, 403, "delete gate ({status}): {body}");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sync/replicate_reconciliation",
        &json!({"source_system": "cnet"}),
        &manager,
    )
    .await;
    assert_ne!(
        status, 403,
        "the non-destructive migration stays manager-level ({status}): {body}"
    );

    let admin = crate::common::keycloak::get_keycloak_jwt("admin", "admin").await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sync/replicate_reconciliation/delete",
        &json!({"source_system": "cnet"}),
        &admin,
    )
    .await;
    assert_ne!(status, 403, "an administrator passes ({status}): {body}");
}

/// A manager granted only another project neither sees nor acts on a hold whose stream is paired
/// into this one, and an unpaired (deferred) hold is out of every restricted caller's reach.
#[tokio::test]
#[serial]
async fn hold_review_is_confined_to_the_callers_projects() {
    if !crate::common::keycloak::keycloak_reachable().await {
        eprintln!("SKIP: keycloak unreachable (start the dev stack, or set TEST_KEYCLOAK_URL)");
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::keycloak::build_test_app_with_keycloak(db.clone()).await;

    let other_project = "00000000-0000-4000-a000-0000000000c1";
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) VALUES \
             ('{other_project}', 'Elsewhere', 'scope check', 'other')"
        ),
    )
    .await;

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "scopesrc", "source_key": "scoped", "measurement_type": "spot"}),
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

    let readings: Vec<serde_json::Value> = [(0i16, 10.0), (1, 20.0), (2, 30.0)]
        .iter()
        .map(|(i, v)| json!({"time": T1, "raw_value": v, "replicate_index": i}))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream,
            "readings": readings,
            "audit": [{"time": T1, "expected_mean": 25.0}],
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "audited ingest ({status}): {body}");
    let (status, holds) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/replicate_audit_holds?stream_id={stream}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "list ({status}): {holds}");
    let hold_id = holds["holds"][0]["id"].as_str().unwrap().to_string();

    // manager1 holds the manager capability globally but is granted only the other project.
    let sub = crate::common::keycloak::keycloak_user_id("manager1").await;
    crate::common::keycloak::grant_project(&db, &sub, other_project).await;
    let jwt = crate::common::keycloak::get_keycloak_jwt("manager1", "manager1").await;

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/sync/replicate_audit_holds", &jwt).await;
    assert_eq!(status, 200, "list ({status}): {body}");
    assert_eq!(
        body["total"], 0,
        "the other project's hold is invisible: {body}"
    );
    assert_eq!(body["pending"], 0, "{body}");

    for path in [
        format!("/api/sync/replicate_audit_holds/{hold_id}/acknowledge"),
        format!("/api/sync/replicate_audit_holds/{hold_id}/resolve"),
        format!("/api/sync/replicate_audit_holds/{hold_id}/reopen"),
    ] {
        let (status, body) =
            crate::common::post_json_with_token(&app, &path, &json!({"mode": "ours"}), &jwt).await;
        assert_eq!(status, 403, "{path} ({status}): {body}");
    }
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/replicate_audit_holds/acknowledge_bulk",
        &json!({"stream_id": stream}),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "bulk acknowledge ({status}): {body}");
    assert_eq!(
        body["acknowledged"], 0,
        "bulk acknowledge cannot reach the other project's holds: {body}"
    );

    // Granted the hold's own project, the same manager sees and resolves it. The grants cache
    // TTL is 1s in the test config; wait it out so the new grant is read.
    crate::common::keycloak::grant_project(&db, &sub, crate::common::PROJECT_ID).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/sync/replicate_audit_holds", &jwt).await;
    assert_eq!(status, 200, "list ({status}): {body}");
    assert_eq!(body["total"], 1, "{body}");
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/resolve"),
        &json!({"mode": "ours"}),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "resolve in scope ({status}): {body}");
}
