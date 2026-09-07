//! `POST /actions/retag_sd_estimator` is the operator's route to the `sd_estimator_retag` job's
//! own options: a slot declaration leaves the samples a person chose an estimator for at one
//! instant alone, and this route is how those are brought into line afterwards.

use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_TEMP_ID, PARAM_S1_TEMP_ID, SITE1_ID};

const T1: &str = "2025-06-01T10:00:00Z";
const T2: &str = "2025-06-02T10:00:00Z";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn save_group(app: &axum::Router, token: &str, time: &str, values: &[f64]) {
    let readings: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({ "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": v, "time": time, "replicate_index": i })
        })
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/grab_samples",
        &json!({ "site_id": SITE1_ID, "readings": readings }),
        token,
    )
    .await;
    assert_eq!(status, 200, "grab save: {body}");
}


async fn sample_at(db: &sea_orm::DatabaseConnection, time: &str) -> (String, String, f64) {
    use sea_orm::{ConnectionTrait, Statement};
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT sd_estimator, sd_estimator_source, stdev FROM samples \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
                   AND collected_at = '{time}'"
            ),
        ))
        .await
        .unwrap()
        .expect("a sample at the instant");
    (
        row.try_get::<String>("", "sd_estimator").unwrap(),
        row.try_get::<String>("", "sd_estimator_source").unwrap(),
        row.try_get::<f64>("", "stdev").unwrap(),
    )
}

async fn retag(
    app: &axum::Router,
    token: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(app, "/api/actions/retag_sd_estimator", &body, token)
        .await
}

async fn declare(app: &axum::Router, token: &str, estimator: &str) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/site_parameters/{PARAM_S1_TEMP_ID}/declare_sd_estimator"),
        &json!({ "estimator": estimator }),
        token,
    )
    .await;
    assert_eq!(status, 200, "declare: {body}");
    body
}

/// Scenario: one instant carries an estimator a person chose for it; the slot then declares the
/// other divisor. Expected behaviour: the declaration's retag skips that instant, and the retag
/// route with `override_instants` is what brings it into line, recomputing its sd.
#[tokio::test]
#[serial]
async fn override_instants_retags_an_instant_decision() {
    let (db, app, token) = setup().await;
    // 10, 20: sample sd 7.0711, population sd 5.0.
    save_group(&app, &token, T1, &[10.0, 20.0]).await;
    save_group(&app, &token, T2, &[10.0, 20.0]).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE samples SET sd_estimator = 'sample', sd_estimator_source = 'sample' \
             WHERE site_id = '{SITE1_ID}' AND collected_at = '{T1}'"
        ),
    )
    .await;

    let declared = declare(&app, &token, "population").await;
    assert_eq!(declared["samples_affected"], 1, "{declared}");
    let job_id = declared["job_id"].as_str().unwrap();
    assert_eq!(crate::common::e2e::poll_job(&app, &token, job_id, 30).await, "completed");
    assert_eq!(sample_at(&db, T2).await.0, "population");
    let (est, source, sd) = sample_at(&db, T1).await;
    assert_eq!((est.as_str(), source.as_str()), ("sample", "sample"), "the instant decision stands");
    assert!((sd - 7.071_067_811_865_476).abs() < 1e-9, "{sd}");

    let (status, body) = retag(
        &app,
        &token,
        json!({
            "estimator": "population",
            "site_parameter_ids": [PARAM_S1_TEMP_ID],
            "override_instants": true,
        }),
    )
    .await;
    assert_eq!(status, 200, "retag ({status}): {body}");
    assert_eq!(body["samples_affected"], 1, "{body}");
    assert_eq!(body["instant_decisions"], 1, "{body}");
    let job_id = body["job_id"].as_str().expect("a tracked job");
    assert_eq!(crate::common::e2e::poll_job(&app, &token, job_id, 30).await, "completed");

    let (est, source, sd) = sample_at(&db, T1).await;
    assert_eq!((est.as_str(), source.as_str()), ("population", "slot"));
    assert!((sd - 5.0).abs() < 1e-9, "recomputed under the population divisor: {sd}");
}

/// Scenario: the same instant decision, but the retag names a window that excludes it.
/// Expected behaviour: nothing in scope disagrees, no job is enqueued, and the count says so.
#[tokio::test]
#[serial]
async fn a_window_confines_the_retag() {
    let (db, app, token) = setup().await;
    save_group(&app, &token, T1, &[10.0, 20.0]).await;
    save_group(&app, &token, T2, &[10.0, 20.0]).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE samples SET sd_estimator = 'sample', sd_estimator_source = 'sample' \
             WHERE site_id = '{SITE1_ID}' AND collected_at = '{T1}'"
        ),
    )
    .await;
    let declared = declare(&app, &token, "population").await;
    let job_id = declared["job_id"].as_str().unwrap();
    assert_eq!(crate::common::e2e::poll_job(&app, &token, job_id, 30).await, "completed");

    let (status, body) = retag(
        &app,
        &token,
        json!({
            "estimator": "population",
            "site_parameter_ids": [PARAM_S1_TEMP_ID],
            "override_instants": true,
            "start": T2,
        }),
    )
    .await;
    assert_eq!(status, 200, "retag ({status}): {body}");
    assert_eq!(body["samples_affected"], 0, "{body}");
    assert!(body.get("job_id").is_none(), "nothing to do, no job: {body}");
    assert_eq!(sample_at(&db, T1).await.0, "sample");
}

/// The route applies a declaration, it does not make one: a target that is not what the slot
/// declares would stamp `sd_estimator_source = 'slot'` on samples the slot never chose.
#[tokio::test]
#[serial]
async fn a_target_the_slot_does_not_declare_is_refused() {
    let (_db, app, token) = setup().await;
    save_group(&app, &token, T1, &[10.0, 20.0]).await;

    let (status, body) = retag(
        &app,
        &token,
        json!({ "estimator": "population", "site_parameter_ids": [PARAM_S1_TEMP_ID] }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains("declare")),
        "the refusal says to declare first: {body}"
    );

    let (status, body) = retag(&app, &token, json!({ "estimator": "population" })).await;
    assert_eq!(status, 400, "no scope: {body}");

    // A dry run is the preview before declaring: counts, no check, no job.
    let (status, body) = retag(
        &app,
        &token,
        json!({ "estimator": "population", "site_parameter_ids": [PARAM_S1_TEMP_ID], "dry_run": true }),
    )
    .await;
    assert_eq!(status, 200, "dry run: {body}");
    assert_eq!(body["samples_affected"], 1, "{body}");
    assert!(body.get("job_id").is_none(), "a dry run enqueues nothing: {body}");
}
