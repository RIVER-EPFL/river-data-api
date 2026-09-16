//! `POST /readings/sample_preview` says what a replicate group's statistics become without the
//! replicates about to be flagged or with the ones about to be restored, and writes nothing.
//!
//! Run: cargo test --test readings sample_preview -- --test-threads=1

use serde_json::json;
use serial_test::serial;

use crate::common::e2e::count;
use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const T: &str = "2025-06-01T10:00:00Z";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let readings: Vec<_> = [10.0, 20.0, 30.0]
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({ "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": v, "time": T, "replicate_index": i })
        })
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({ "site_id": SITE1_ID, "readings": readings }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab save: {body}");
    (db, app, token)
}

async fn preview(
    app: &axum::Router,
    token: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(app, "/api/readings/sample_preview", &body, token)
        .await
}

fn near(v: &serde_json::Value, expected: f64) -> bool {
    v.as_f64().is_some_and(|v| (v - expected).abs() < 1e-9)
}

#[tokio::test]
#[serial]
async fn excluding_the_highest_replicate_previews_the_mean_of_the_other_two() {
    let (db, app, token) = setup().await;

    let (status, body) = preview(
        &app,
        &token,
        json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": T,
            "exclude_replicate_indexes": [2],
        }),
    )
    .await;
    assert_eq!(status, 200, "preview ({status}): {body}");
    assert_eq!(body["current"]["n"], 3);
    assert!(near(&body["current"]["mean"], 20.0), "{body}");
    assert_eq!(body["proposed"]["n"], 2);
    assert!(near(&body["proposed"]["mean"], 15.0), "{body}");
    assert!(near(&body["delta"]["mean"], -5.0), "{body}");
    assert_eq!(body["delta"]["n"], -1);
    assert_eq!(body["replicates"][2]["included_after"], false);

    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM readings WHERE is_flagged = TRUE").await,
        0,
        "a preview flags nothing"
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM reading_decisions").await,
        0,
        "a preview records no decision"
    );
    let stored_mean = db_mean(&db).await;
    assert!(
        (stored_mean - 20.0).abs() < 1e-9,
        "the sample is untouched: {stored_mean}"
    );
}

/// Scenario: a client still asks for the sd under another divisor.
///
/// Expected behaviour: the request is refused rather than answered with the sample sd, because the
/// preview has one divisor and a field it does not read must not look honoured.
#[tokio::test]
#[serial]
async fn a_divisor_in_the_request_is_refused() {
    let (_db, app, token) = setup().await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/sample_preview",
        &json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": T,
            "estimator": "population",
        }),
        &token,
    )
    .await;
    assert!((400..500).contains(&status), "refused ({status}): {body}");
}

#[tokio::test]
#[serial]
async fn an_index_the_group_does_not_hold_is_refused() {
    let (_db, app, token) = setup().await;
    let (status, body) = preview(
        &app,
        &token,
        json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": T,
            "exclude_replicate_indexes": [7],
        }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains('7')),
        "{body}"
    );
}

async fn db_mean(db: &sea_orm::DatabaseConnection) -> f64 {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT mean FROM samples WHERE site_id = '{SITE1_ID}' \
             AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' AND collected_at = '{T}'"
        ),
    ))
    .await
    .unwrap()
    .expect("a sample")
    .try_get::<f64>("", "mean")
    .unwrap()
}
