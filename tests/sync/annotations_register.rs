//! `/annotations/register`: provenance-keyed upsert of source-authored annotations. The site and
//! parameter come from the stream's pairing; re-asserting the same key is idempotent, an edited
//! text updates in place, and an unpaired stream's annotation is refused per item as `unpaired`.
//!
//! Run: cargo test --test sync annotations_register -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-06-10T08:00:00Z";

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    Fixture { db, app, token }
}

async fn register_stream(fx: &Fixture, key: &str, pair: bool) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": key, "measurement_type": "spot"}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "register: {stream}");
    let stream_id = crate::common::e2e::id_of(&stream);
    if pair {
        let (status, body) = crate::common::post_json_with_token(
            &fx.app,
            &format!("/api/streams/{stream_id}/pair"),
            &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
            &fx.token,
        )
        .await;
        assert_eq!(status, 200, "pair ({status}): {body}");
    }
    stream_id
}

async fn register(fx: &Fixture, stream_id: &str, source_key: &str, text: &str) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/annotations/register",
        &json!({"source_system": "cnet", "annotations": [
            {"source_key": source_key, "stream_id": stream_id, "time": T1,
             "category": "sync", "text": text}
        ]}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "register annotations ({status}): {body}");
    body["annotations"][0].clone()
}

#[tokio::test]
#[serial]
async fn upsert_is_idempotent_and_pairing_resolves_the_slot() {
    let fx = setup().await;
    let stream = register_stream(&fx, "FP1:DOC:reps", true).await;
    let key = "FP1:doc_std_curve_id:2025-06-10T08:00:00Z";

    let first = register(&fx, &stream, key, "Corrected at source with curve 'DOC corr'").await;
    assert_eq!(first["status"], "created", "{first}");
    let id = first["id"].as_str().unwrap().to_string();

    let row = fx
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT site_id::text AS site_id, parameter_id::text AS parameter_id,
                        (start_time = end_time) AS point, category, created_by
                 FROM annotations WHERE id = '{id}'"
            ),
        ))
        .await
        .unwrap()
        .expect("annotation stored");
    assert_eq!(
        row.try_get::<String>("", "site_id").unwrap(),
        crate::common::SITE1_ID,
        "site comes from the pairing, not the request"
    );
    assert_eq!(
        row.try_get::<String>("", "parameter_id").unwrap(),
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "parameter comes from the pairing"
    );
    assert!(row.try_get::<bool>("", "point").unwrap(), "stored as a point");
    assert_eq!(row.try_get::<String>("", "category").unwrap(), "sync");
    assert_eq!(
        row.try_get::<Option<String>>("", "created_by").unwrap(),
        Some("sync:cnet".to_string())
    );

    let again = register(&fx, &stream, key, "Corrected at source with curve 'DOC corr'").await;
    assert_eq!(again["status"], "unchanged", "{again}");
    assert_eq!(again["id"].as_str().unwrap(), id, "same row on re-assert");

    let edited = register(&fx, &stream, key, "Corrected at source with curve 'DOC corr' v2").await;
    assert_eq!(edited["status"], "updated", "{edited}");
    assert_eq!(edited["id"].as_str().unwrap(), id, "an edit updates in place");

    let n = crate::common::e2e::count(
        &fx.db,
        "SELECT COUNT(*) FROM annotations WHERE source_system = 'cnet'",
    )
    .await;
    assert_eq!(n, 1, "three passes, one row");
}

#[tokio::test]
#[serial]
async fn unpaired_stream_is_refused_per_item_and_stores_nothing() {
    let fx = setup().await;
    let unpaired = register_stream(&fx, "FP2:DOC:reps", false).await;

    let outcome = register(&fx, &unpaired, "FP2:doc_std_curve_id:x", "text").await;
    assert_eq!(outcome["status"], "unpaired", "{outcome}");
    assert!(outcome["id"].is_null());
    assert_eq!(
        crate::common::e2e::count(
            &fx.db,
            "SELECT COUNT(*) FROM annotations WHERE source_system = 'cnet'"
        )
        .await,
        0
    );
}

#[tokio::test]
#[serial]
async fn summary_counts_annotated_points_and_csv_exports() {
    let fx = setup().await;
    let stream = register_stream(&fx, "FP1:DOC:reps", true).await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({"stream_id": stream, "readings": [
            {"time": "2025-06-10T08:00:00Z", "raw_value": 10.0, "replicate_index": 0},
            {"time": "2025-06-10T09:00:00Z", "raw_value": 11.0, "replicate_index": 0},
            {"time": "2025-06-10T10:00:00Z", "raw_value": 12.0, "replicate_index": 0}
        ]}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");

    for (key, time) in [
        ("FP1:doc_std_curve_id:2025-06-10T08:00:00Z", "2025-06-10T08:00:00Z"),
        ("FP1:doc_std_curve_id:2025-06-10T09:00:00Z", "2025-06-10T09:00:00Z"),
    ] {
        let (status, body) = crate::common::post_json_parse_with_token(
            &fx.app,
            "/api/annotations/register",
            &json!({"source_system": "cnet", "annotations": [
                {"source_key": key, "stream_id": stream, "time": time,
                 "category": "sync", "text": "Corrected at source with standard curve 'DOC corr'"}
            ]}),
            &fx.token,
        )
        .await;
        assert_eq!(status, 200, "register ({status}): {body}");
    }

    let range = "start=2025-06-10T00:00:00Z&end=2025-06-11T00:00:00Z";
    let (status, summary) = crate::common::get_json_with_token(
        &fx.app,
        &format!("/api/sites/{}/export/summary?{range}", crate::common::SITE1_ID),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "summary ({status}): {summary}");
    assert_eq!(summary["annotation_count"], 2, "{summary}");
    assert_eq!(
        summary["annotated_points"], 2,
        "two of the three instants carry an annotation: {summary}"
    );
    assert_eq!(summary["per_parameter"][0]["annotated_points"], 2);
    assert_eq!(summary["flagged_readings"], 0, "{summary}");
    assert_eq!(summary["replicate_readings"], 0, "{summary}");
    assert_eq!(summary["alarm_readings"], 0, "{summary}");

    let (status, csv) = crate::common::get_csv_with_token(
        &fx.app,
        &format!(
            "/api/sites/{}/annotations?{range}&format=csv",
            crate::common::SITE1_ID
        ),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "csv export ({status})");
    let lines: Vec<&str> = csv.trim().lines().collect();
    assert_eq!(lines.len(), 3, "header plus two annotations: {csv}");
    assert!(
        lines[0].starts_with("site,parameter_code,category,start_time,end_time,text"),
        "{csv}"
    );
    assert!(
        lines[1].contains("sync") && lines[1].contains("cnet"),
        "category and source_system in the row: {csv}"
    );
}
