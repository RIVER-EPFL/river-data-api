//! `/ingest` accepting a per-reading `standard_curve_id`: the stored value is computed from the
//! curve's coefficients on top of any base calibration and the reference is stamped. A claim
//! naming another instrument's curve is stripped, never the reading: the value is stored
//! uncorrected and the claim lands in the review queue as a `curve_claim_stripped` hold.
//!
//! Run: cargo test --test readings ingest_standard_curves -- --test-threads=1

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

/// Register a portal curve; returns (curve_id, lab_sensor_id).
async fn register_curve(fx: &Fixture, source_key: &str, label: &str) -> (String, String) {
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves/register",
        &json!({
            "source_system": "cnet",
            "source_key": source_key,
            "instrument_label": label,
            "slope": 2.0,
            "intercept": 1.0,
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "register curve ({status}): {body}");
    (
        body["id"].as_str().unwrap().to_string(),
        body["sensor_id"].as_str().unwrap().to_string(),
    )
}

async fn register_paired_spot_stream(fx: &Fixture, key: &str, sensor_id: &str) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": key, "measurement_type": "spot",
                "sensor_id": sensor_id}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "register: {stream}");
    let stream_id = crate::common::e2e::id_of(&stream);
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");
    stream_id
}

async fn scalar_f64(db: &DatabaseConnection, sql: &str) -> f64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<f64>("", "v")
    .unwrap()
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    crate::common::e2e::count(db, sql).await
}

#[tokio::test]
#[serial]
async fn curve_stamped_replicates_match_portal_mean() {
    let fx = setup().await;
    let (curve, lab_sensor) = register_curve(&fx, "standard_curves:17", "DOC corr").await;
    let stream = register_paired_spot_stream(&fx, "DGT:DOC:reps", &lab_sensor).await;

    let readings: Vec<serde_json::Value> = [10.0, 20.0, 30.0]
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({"time": T1, "raw_value": v, "replicate_index": i,
                   "standard_curve_id": curve})
        })
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({"stream_id": stream, "readings": readings}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    assert_eq!(body["inserted"], 3);
    assert_eq!(body["skipped"], 0, "no admissible claim skipped: {body}");

    for (idx, raw) in [(0, 10.0), (1, 20.0), (2, 30.0)] {
        let calibrated = scalar_f64(
            &fx.db,
            &format!(
                "SELECT calibrated_value AS v FROM readings \
                 WHERE stream_id = '{stream}' AND time = '{T1}' AND replicate_index = {idx}"
            ),
        )
        .await;
        let expected = 2.0 * raw + 1.0;
        assert!(
            (calibrated - expected).abs() < 1e-9,
            "replicate {idx}: {calibrated} != slope*raw+intercept = {expected}"
        );
    }
    assert_eq!(
        count(
            &fx.db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{stream}' \
                 AND standard_curve_id = '{curve}'"
            ),
        )
        .await,
        3,
        "every replicate carries the curve it was corrected with"
    );

    let mean = scalar_f64(
        &fx.db,
        &format!(
            "SELECT mean AS v FROM samples WHERE site_id = '{}' AND parameter_id = '{}' \
             AND collected_at = '{T1}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert!(
        (mean - 41.0).abs() < 1e-9,
        "samples.mean is the mean of the corrected values (21, 41, 61): {mean}"
    );
}

#[tokio::test]
#[serial]
async fn wrong_instrument_curve_stripped_and_held() {
    let fx = setup().await;
    let (_doc_curve, doc_sensor) = register_curve(&fx, "standard_curves:17", "DOC corr").await;
    let (tn_curve, _tn_sensor) = register_curve(&fx, "standard_curves:18", "TN corr").await;
    let stream = register_paired_spot_stream(&fx, "DGT:DOC2:reps", &doc_sensor).await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({"stream_id": stream, "readings": [
            {"time": T1, "raw_value": 10.0, "replicate_index": 0, "standard_curve_id": tn_curve}
        ]}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    assert_eq!(body["inserted"], 1, "the reading is stored, only the claim is refused");
    assert_eq!(body["skipped"], 0);

    let row = fx
        .db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, calibrated_value, standard_curve_id FROM readings \
                 WHERE stream_id = '{stream}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the reading is stored");
    assert_eq!(row.try_get::<f64>("", "raw_value").unwrap(), 10.0);
    assert!(
        row.try_get::<Option<f64>>("", "calibrated_value")
            .unwrap()
            .is_none(),
        "stored uncorrected: no curve was verifiably applicable"
    );
    assert!(
        row.try_get::<Option<uuid::Uuid>>("", "standard_curve_id")
            .unwrap()
            .is_none(),
        "the inadmissible claim is not stamped"
    );

    let hold = fx
        .db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT status, expected FROM replicate_audit_holds \
                 WHERE stream_id = '{stream}' AND kind = 'curve_claim_stripped'"
            ),
        ))
        .await
        .unwrap()
        .expect("the stripped claim raises a hold");
    assert_eq!(hold.try_get::<String>("", "status").unwrap(), "pending");
    let expected = hold.try_get::<serde_json::Value>("", "expected").unwrap();
    let claim = &expected["claims"][0];
    assert_eq!(claim["standard_curve_id"], json!(tn_curve));
    assert_eq!(
        claim["reason"],
        json!("fitted on a different instrument than the reading's")
    );
}
