//! `POST /standard_curves/register`: provenance-keyed idempotent upsert of portal standard
//! curves. Re-registration resolves the same row and the same lab instrument; changed
//! coefficients update an unused curve in place; a curve any reading references is frozen, so an
//! upstream edit mints a successor that takes over the provenance while history keeps the old row.
//!
//! Run: cargo test --test sensors standard_curve_register -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
}

async fn setup() -> Fixture {
    let f = crate::common::seeded_app().await;
    Fixture { db: f.db, app: f.app, token: f.token }
}

async fn register(fx: &Fixture, slope: f64, intercept: f64) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves/register",
        &json!({
            "source_system": "cnet",
            "source_key": "standard_curves:17",
            "instrument_label": "DOC corr",
            "slope": slope,
            "intercept": intercept,
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "register ({status}): {body}");
    body
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    crate::common::e2e::count(db, sql).await
}

async fn stored_slope(db: &DatabaseConnection, curve_id: &str) -> f64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT slope AS v FROM standard_curves WHERE id = '{curve_id}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<f64>("", "v")
    .unwrap()
}

/// A reading corrected by the curve, on a throwaway stream; what freezes the coefficients.
async fn reference_curve_from_a_reading(db: &DatabaseConnection, curve_id: &str) {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'cnet', '{}', 'lab feed', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, time, replicate_index, raw_value, calibrated_value, \
                                   measurement_type, standard_curve_id) \
             VALUES ('{stream_id}', '2025-06-01T08:00:00Z', 0, 10.0, 21.0, 'spot', '{curve_id}')"
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn register_upserts_by_provenance() {
    let fx = setup().await;

    let first = register(&fx, 2.0, 1.0).await;
    assert_eq!(first["superseded"], false);
    let curve_id = first["id"].as_str().unwrap().to_string();
    let sensor_id = first["sensor_id"].as_str().unwrap().to_string();

    assert_eq!(
        count(
            &fx.db,
            &format!(
                "SELECT COUNT(*) FROM sensors WHERE id = '{sensor_id}' \
                 AND source_system = 'cnet' AND source_key = 'cnet:DOC corr' \
                 AND serial_number IS NULL AND is_lab_instrument"
            ),
        )
        .await,
        1,
        "one lab instrument minted per (source_system, instrument_label), identified by its \
         provenance pair; serial_number holds a real instrument serial or nothing"
    );

    let second = register(&fx, 2.0, 1.0).await;
    assert_eq!(second["id"], first["id"], "same coefficients, same curve");
    assert_eq!(second["sensor_id"], first["sensor_id"]);
    assert_eq!(second["superseded"], false);

    assert_eq!(
        count(&fx.db, "SELECT COUNT(*) FROM standard_curves").await,
        1,
        "re-registration mints nothing"
    );
    assert_eq!(
        count(
            &fx.db,
            "SELECT COUNT(*) FROM sensors WHERE is_lab_instrument"
        )
        .await,
        1,
        "re-registration resolves the existing instrument"
    );
    assert!(
        (stored_slope(&fx.db, &curve_id).await - 2.0).abs() < 1e-12,
        "the stored coefficients are the registered ones"
    );
}

#[tokio::test]
#[serial]
async fn changed_coefficients_update_unused_curve() {
    let fx = setup().await;

    let first = register(&fx, 2.0, 1.0).await;
    let curve_id = first["id"].as_str().unwrap().to_string();

    let second = register(&fx, 3.0, 1.0).await;
    assert_eq!(
        second["id"], first["id"],
        "an unused curve is corrected in place under the same id"
    );
    assert_eq!(second["superseded"], false);
    assert_eq!(
        count(&fx.db, "SELECT COUNT(*) FROM standard_curves").await,
        1
    );
    assert!(
        (stored_slope(&fx.db, &curve_id).await - 3.0).abs() < 1e-12,
        "the coefficients moved with the portal"
    );
}

#[tokio::test]
#[serial]
async fn used_curve_edit_mints_successor() {
    let fx = setup().await;

    let first = register(&fx, 2.0, 1.0).await;
    let old_id = first["id"].as_str().unwrap().to_string();
    reference_curve_from_a_reading(&fx.db, &old_id).await;

    let second = register(&fx, 3.0, 0.5).await;
    assert_eq!(
        second["superseded"], true,
        "an edit to a used curve supersedes it"
    );
    let new_id = second["id"].as_str().unwrap().to_string();
    assert_ne!(new_id, old_id, "the successor is a new row");
    assert_eq!(second["sensor_id"], first["sensor_id"], "same instrument");

    assert_eq!(
        count(
            &fx.db,
            &format!(
                "SELECT COUNT(*) FROM standard_curves WHERE id = '{old_id}' \
                 AND source_system IS NULL AND source_key IS NULL \
                 AND slope = 2.0 AND intercept = 1.0"
            ),
        )
        .await,
        1,
        "the old row keeps its coefficients and loses only its provenance"
    );
    assert_eq!(
        count(
            &fx.db,
            &format!(
                "SELECT COUNT(*) FROM standard_curves WHERE id = '{new_id}' \
                 AND source_system = 'cnet' AND source_key = 'standard_curves:17' \
                 AND slope = 3.0 AND intercept = 0.5"
            ),
        )
        .await,
        1,
        "the successor carries the provenance and the new coefficients"
    );
    assert_eq!(
        count(
            &fx.db,
            &format!("SELECT COUNT(*) FROM readings WHERE standard_curve_id = '{old_id}'"),
        )
        .await,
        1,
        "history still references the curve that produced it"
    );

    let third = register(&fx, 3.0, 0.5).await;
    assert_eq!(
        third["id"].as_str().unwrap(),
        new_id,
        "re-registration resolves the successor"
    );
    assert_eq!(third["superseded"], false);
}

/// An annotation recording the curve as the source-side correction, the reference a corrected
/// column leaves when no reading may carry the curve.
async fn reference_curve_from_an_annotation(fx: &Fixture, curve_id: &str) {
    let (status, stream) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "FP1:chla", "measurement_type": "spot"}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "register stream: {stream}");
    let stream_id = crate::common::e2e::id_of(&stream);
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/annotations/register",
        &json!({"source_system": "cnet", "annotations": [
            {"source_key": "FP1:chla_std_curve_id:2025-06-01T08:00:00Z", "stream_id": stream_id,
             "time": "2025-06-01T08:00:00Z", "category": "sync",
             "text": "Corrected at source", "standard_curve_id": curve_id}
        ]}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "register annotation ({status}): {body}");
    assert_eq!(body["annotations"][0]["status"], "created", "{body}");
}

#[tokio::test]
#[serial]
async fn a_curve_referenced_only_by_an_annotation_is_used() {
    let fx = setup().await;
    let first = register(&fx, 2.0, 1.0).await;
    let old_id = first["id"].as_str().unwrap().to_string();
    reference_curve_from_an_annotation(&fx, &old_id).await;

    let second = register(&fx, 3.0, 0.5).await;
    assert_eq!(second["superseded"], true, "{second}");
    assert_ne!(second["id"].as_str().unwrap(), old_id);
    assert!(
        (stored_slope(&fx.db, &old_id).await - 2.0).abs() < 1e-12,
        "the curve the annotation names keeps the coefficients it recorded"
    );
}

async fn stored_fitted_on(db: &DatabaseConnection, curve_id: &str) -> Option<String> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT fitted_on::text AS v FROM standard_curves WHERE id = '{curve_id}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<Option<String>>("", "v")
    .unwrap()
}

/// Expected behaviour: a curve is identified in the lab by the date it was fitted, so the source's
/// own date is stored rather than folded into free text, and a source that reports none leaves the
/// row on its creation date rather than on nothing.
#[tokio::test]
#[serial]
async fn fitted_on_travels_from_the_source() {
    let fx = setup().await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves/register",
        &json!({
            "source_system": "cnet",
            "source_key": "standard_curves:17",
            "instrument_label": "DOC corr",
            "slope": 2.0,
            "intercept": 1.0,
            "fitted_on": "2021-01-28",
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "register ({status}): {body}");
    let dated = body["id"].as_str().unwrap().to_string();
    assert_eq!(
        stored_fitted_on(&fx.db, &dated).await.as_deref(),
        Some("2021-01-28")
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves/register",
        &json!({
            "source_system": "cnet",
            "source_key": "standard_curves:18",
            "instrument_label": "DOC corr",
            "slope": 2.0,
            "intercept": 1.0,
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "register ({status}): {body}");
    let undated = body["id"].as_str().unwrap().to_string();
    assert!(
        stored_fitted_on(&fx.db, &undated).await.is_some(),
        "a source reporting no date leaves the curve on its creation date, never on NULL"
    );
}
