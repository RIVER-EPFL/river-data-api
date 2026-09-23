//! A grab save in `replace` mode rewrites only what the grab stream owns at the instant: rows
//! another stream wrote at the same (site, parameter, time) survive, and a curated row on the grab
//! stream (flagged, withdrawn, or carrying a hand-picked standard curve the request does not
//! supply) is kept and raises a `source_modified` hold instead of being deleted. A replicate the
//! save does not carry at all is withdrawn, reversibly, rather than deleted.
//!
//! Run: cargo test --test readings grab_replace_scope -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T: &str = "2025-06-01T08:00:00Z";

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
}

async fn setup() -> Fixture {
    let f = crate::common::seeded_app().await;
    Fixture {
        db: f.db,
        app: f.app,
        token: f.token,
    }
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

fn grab(values: &[f64], mode: Option<&str>) -> serde_json::Value {
    let readings: Vec<serde_json::Value> = values
        .iter()
        .map(
            |v| json!({"parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "value": v, "time": T}),
        )
        .collect();
    let mut body =
        json!({"site_id": crate::common::SITE1_ID, "readings": readings});
    if let Some(m) = mode {
        body["mode"] = json!(m);
    }
    body
}

async fn save(fx: &Fixture, body: &serde_json::Value) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(&fx.app, "/api/grab_samples", body, &fx.token).await
}

fn grab_rows_where(extra: &str) -> String {
    format!(
        "readings r JOIN data_streams s ON s.id = r.stream_id \
         WHERE s.source_system = 'grab_sample' AND r.site_id = '{}' AND r.parameter_id = '{}' \
         AND r.time = '{T}' {extra}",
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID
    )
}

async fn grab_rows(fx: &Fixture) -> Vec<(i16, f64)> {
    let rows = fx
        .db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT r.replicate_index, r.raw_value FROM {} ORDER BY 1",
                grab_rows_where("")
            ),
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| {
            (
                r.try_get::<i16>("", "replicate_index").unwrap(),
                r.try_get::<f64>("", "raw_value").unwrap(),
            )
        })
        .collect()
}

async fn holds_on_grab_stream(fx: &Fixture) -> Vec<(String, String, serde_json::Value)> {
    let rows = fx
        .db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT h.kind, h.status, h.expected FROM replicate_audit_holds h \
                 JOIN data_streams s ON s.id = h.stream_id \
                 WHERE s.source_system = 'grab_sample' AND h.group_time = '{T}'"
            ),
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| {
            (
                r.try_get::<String>("", "kind").unwrap(),
                r.try_get::<String>("", "status").unwrap(),
                r.try_get::<serde_json::Value>("", "expected").unwrap(),
            )
        })
        .collect()
}

async fn seed_portal_group(fx: &Fixture) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "S1:temp:reps", "measurement_type": "spot"}),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register ({status}): {stream}"
    );
    let stream_id = crate::common::e2e::id_of(&stream);
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");
    let (sync_token, _) = crate::common::seed_sync_session_token(&fx.db).await;
    let readings: Vec<_> = [1.0, 2.0, 3.0]
        .iter()
        .enumerate()
        .map(|(i, v)| json!({"time": T, "raw_value": v, "replicate_index": i}))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({"stream_id": stream_id, "readings": readings, "collection": true}),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    stream_id
}

async fn portal_rows(fx: &Fixture, stream_id: &str) -> i64 {
    scalar_i64(
        &fx.db,
        &format!(
            "SELECT COUNT(*) AS n FROM readings WHERE stream_id = '{stream_id}' AND time = '{T}'"
        ),
    )
    .await
}

#[tokio::test]
#[serial]
async fn replace_leaves_another_streams_rows_at_the_instant() {
    let fx = setup().await;
    let portal = seed_portal_group(&fx).await;

    let (status, body) = save(&fx, &grab(&[10.0, 20.0, 30.0], None)).await;
    assert_eq!(
        status, 409,
        "the portal group is reported as stored ({status}): {body}"
    );

    let (status, body) = save(&fx, &grab(&[10.0, 20.0, 30.0], Some("replace"))).await;
    assert_eq!(status, 200, "replace ({status}): {body}");
    assert_eq!(
        body["replaced"], 0,
        "nothing on the grab stream to replace yet: {body}"
    );
    assert_eq!(body["inserted"], 3);
    assert_eq!(
        portal_rows(&fx, &portal).await,
        3,
        "the portal replicates survive"
    );

    let (status, body) = save(&fx, &grab(&[40.0, 60.0], Some("replace"))).await;
    assert_eq!(status, 200, "second replace ({status}): {body}");
    assert_eq!(
        body["replaced"], 2,
        "only the grab stream's own rows are replaced: {body}"
    );
    assert_eq!(
        body["withdrawn"], 1,
        "the replicate the save dropped is withdrawn, not deleted: {body}"
    );
    assert_eq!(body["inserted"], 2);
    assert_eq!(
        portal_rows(&fx, &portal).await,
        3,
        "the portal replicates still survive"
    );
    assert_eq!(grab_rows(&fx).await, vec![(0, 40.0), (1, 60.0), (2, 30.0)]);
    assert!(
        holds_on_grab_stream(&fx).await.is_empty(),
        "nothing curated, no hold"
    );
}

#[tokio::test]
#[serial]
async fn replace_keeps_a_flagged_replicate_and_raises_a_hold() {
    let fx = setup().await;
    let (status, _) = save(&fx, &grab(&[10.0, 20.0, 30.0], None)).await;
    assert_eq!(status, 200);
    let (status, body) = crate::common::patch_json_with_token(
        &fx.app,
        "/api/readings/flag",
        &json!({"reason": "outlier", "readings": [{
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
            "time": T,
            "replicate_index": 2,
        }]}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "flag ({status}): {body}");

    let (status, body) = save(&fx, &grab(&[40.0, 50.0, 60.0], Some("replace"))).await;
    assert_eq!(status, 200, "replace ({status}): {body}");
    assert_eq!(
        body["replaced"], 2,
        "the flagged replicate is not removed: {body}"
    );
    assert_eq!(body["kept_curated"], 1, "and is reported as kept: {body}");
    assert_eq!(
        body["inserted"], 2,
        "the value entered at the kept index is not written: {body}"
    );
    assert_eq!(grab_rows(&fx).await, vec![(0, 40.0), (1, 50.0), (2, 30.0)]);
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!(
                "SELECT COUNT(*) AS n FROM {}",
                grab_rows_where("AND r.is_flagged")
            )
        )
        .await,
        1,
        "the flag survives"
    );

    let holds = holds_on_grab_stream(&fx).await;
    assert_eq!(holds.len(), 1, "one hold per group: {holds:?}");
    assert_eq!(holds[0].0, "source_modified");
    assert_eq!(holds[0].1, "pending");
    assert_eq!(holds[0].2["claim"], "replaced", "{:?}", holds[0].2);
    assert_eq!(
        holds[0].2["kept"][0]["replicate_index"], 2,
        "{:?}",
        holds[0].2
    );
    assert_eq!(
        holds[0].2["kept"][0]["reason"], "flagged",
        "{:?}",
        holds[0].2
    );
}

#[tokio::test]
#[serial]
async fn replace_keeps_a_withdrawn_replicate() {
    let fx = setup().await;
    let (status, _) = save(&fx, &grab(&[10.0, 20.0, 30.0], None)).await;
    assert_eq!(status, 200);
    crate::common::exec(
        &fx.db,
        &format!(
            "UPDATE readings r SET withdrawn_at = NOW(), withdrawn_reason = 'test' \
             FROM data_streams s WHERE s.id = r.stream_id AND s.source_system = 'grab_sample' \
             AND r.time = '{T}' AND r.replicate_index = 1"
        ),
    )
    .await;

    let (status, body) = save(&fx, &grab(&[40.0, 50.0], Some("replace"))).await;
    assert_eq!(status, 200, "replace ({status}): {body}");
    assert_eq!(body["replaced"], 1, "{body}");
    assert_eq!(body["kept_curated"], 1, "{body}");
    assert_eq!(
        body["withdrawn"], 1,
        "the replicate the save dropped: {body}"
    );
    assert_eq!(grab_rows(&fx).await, vec![(0, 40.0), (1, 20.0), (2, 30.0)]);
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!(
                "SELECT COUNT(*) AS n FROM {}",
                grab_rows_where("AND r.withdrawn_at IS NOT NULL")
            )
        )
        .await,
        2,
        "the withdrawn stamp survives, beside the one this save made"
    );
    let holds = holds_on_grab_stream(&fx).await;
    assert_eq!(holds.len(), 1, "{holds:?}");
    assert_eq!(
        holds[0].2["kept"][0]["reason"], "withdrawn",
        "{:?}",
        holds[0].2
    );
}

#[tokio::test]
#[serial]
async fn replace_keeps_a_hand_curved_replicate_unless_the_request_supplies_a_curve() {
    let fx = setup().await;
    let sensor =
        crate::common::sensor_lifecycle::create_sensor_without_curve(&fx.db, "plate-reader").await;
    let (status, curve) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves",
        &json!({"sensor_id": sensor, "name": "plate 1", "slope": 2.0, "intercept": 0.0}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "curve ({status}): {curve}");
    let curve_id = curve["id"].as_str().unwrap().to_string();

    let (status, _) = save(&fx, &grab(&[10.0, 20.0, 30.0], None)).await;
    assert_eq!(status, 200);
    crate::common::exec(
        &fx.db,
        &format!(
            "UPDATE readings r SET standard_curve_id = '{curve_id}' \
             FROM data_streams s WHERE s.id = r.stream_id AND s.source_system = 'grab_sample' \
             AND r.time = '{T}' AND r.replicate_index = 0"
        ),
    )
    .await;

    let (status, body) = save(&fx, &grab(&[40.0, 50.0, 60.0], Some("replace"))).await;
    assert_eq!(status, 200, "replace without a curve ({status}): {body}");
    assert_eq!(body["replaced"], 2, "{body}");
    assert_eq!(body["kept_curated"], 1, "{body}");
    assert_eq!(grab_rows(&fx).await, vec![(0, 10.0), (1, 50.0), (2, 60.0)]);
    let holds = holds_on_grab_stream(&fx).await;
    assert_eq!(holds.len(), 1, "{holds:?}");
    assert_eq!(
        holds[0].2["kept"][0]["reason"], "standard_curve",
        "{:?}",
        holds[0].2
    );

    let mut with_curve = grab(&[70.0, 80.0, 90.0], Some("replace"));
    for r in with_curve["readings"].as_array_mut().unwrap() {
        r["sensor_id"] = json!(sensor);
        r["standard_curve_id"] = json!(curve_id);
    }
    let (status, body) = save(&fx, &with_curve).await;
    assert_eq!(status, 200, "replace with a curve ({status}): {body}");
    assert_eq!(
        body["replaced"], 3,
        "the request's own curve choice replaces the stored one: {body}"
    );
    assert_eq!(body["kept_curated"], 0, "{body}");
    assert_eq!(grab_rows(&fx).await, vec![(0, 70.0), (1, 80.0), (2, 90.0)]);
}

/// Scenario: a save carries fewer replicates than the instant already holds, because a cell was
/// cleared or a pasted block is one column narrower than what is stored.
///
/// Expected behaviour: the replicates the save does not carry are withdrawn and stay readable,
/// each with a `withdraw` decision to roll back from. Nothing deletes.
#[tokio::test]
#[serial]
async fn replace_withdraws_the_replicates_the_save_does_not_carry() {
    let fx = setup().await;
    let (status, _) = save(&fx, &grab(&[10.0, 20.0, 30.0], None)).await;
    assert_eq!(status, 200);

    let (status, body) = save(&fx, &grab(&[40.0, 60.0], Some("replace"))).await;
    assert_eq!(status, 200, "replace ({status}): {body}");
    assert_eq!(
        body["replaced"], 2,
        "only the rewritten replicates are replaced: {body}"
    );
    assert_eq!(
        body["withdrawn"], 1,
        "the replicate the save dropped is withdrawn: {body}"
    );
    assert_eq!(
        grab_rows(&fx).await,
        vec![(0, 40.0), (1, 60.0), (2, 30.0)],
        "the dropped replicate keeps its value"
    );
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!(
                "SELECT COUNT(*) AS n FROM {}",
                grab_rows_where("AND r.withdrawn_at IS NOT NULL AND r.replicate_index = 2")
            )
        )
        .await,
        1,
        "and is withdrawn rather than deleted"
    );
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!(
                "SELECT COUNT(*) AS n FROM reading_decisions d \
                 JOIN data_streams s ON s.id = d.stream_id \
                 WHERE s.source_system = 'grab_sample' AND d.time = '{T}' \
                   AND d.replicate_index = 2 AND d.kind = 'withdraw'"
            )
        )
        .await,
        1,
        "the retraction is on the ledger, to roll back from"
    );
}
