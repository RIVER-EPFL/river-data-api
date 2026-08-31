//! The declare endpoint is the one path for a slot's sd declaration: it writes the column,
//! counts the stored samples the change touches, and enqueues the tracked retag over them.
//! `sd_estimator` is excluded from CRUD update, so a plain update cannot change the declaration
//! while skipping the recompute.

use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, PARAM_S1_TEMP_ID, SITE1_ID};

const GRAB_TIME: &str = "2025-06-01T10:00:00Z";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn scalar_i64(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "v")
    .unwrap()
}

async fn slot_estimator(db: &sea_orm::DatabaseConnection) -> Option<String> {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT sd_estimator AS v FROM site_parameters WHERE id = '{PARAM_S1_TEMP_ID}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<Option<String>>("", "v")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn declaring_recomputes_stored_samples_and_a_crud_update_cannot() {
    let (db, app, token) = setup().await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 10.0, "time": GRAB_TIME, "replicate_index": 0 },
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 12.0, "time": GRAB_TIME, "replicate_index": 1 },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab save: {body}");

    let (status, declared) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/site_parameters/{PARAM_S1_TEMP_ID}/declare_sd_estimator"),
        &json!({ "estimator": "population" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "declare: {declared}");
    assert_eq!(declared["estimator"], json!("population"));
    assert_eq!(declared["previous"], json!(null));
    assert_eq!(
        declared["samples_affected"], 1,
        "the stored sample disagrees with the new divisor: {declared}"
    );
    let job_id: Uuid = declared["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a recompute was owed, so a job was enqueued: {declared}"))
        .parse()
        .unwrap();
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint AS v FROM reprocessing_jobs \
                 WHERE id = '{job_id}' AND trigger_type = 'sd_estimator_retag'"
            ),
        )
        .await,
        1,
        "the tracked retag exists"
    );
    assert_eq!(slot_estimator(&db).await.as_deref(), Some("population"));

    // The CRUD update excludes the field: the request succeeds, the declaration stands.
    let (status, text) = crate::common::put_json_with_token(
        &app,
        &format!("/api/site_parameters/{PARAM_S1_TEMP_ID}"),
        &json!({ "sd_estimator": "sample" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "CRUD update: {text}");
    assert_eq!(
        slot_estimator(&db).await.as_deref(),
        Some("population"),
        "a CRUD update cannot change the declaration"
    );
}

#[tokio::test]
#[serial]
async fn clearing_returns_the_previous_declaration_and_recomputes_nothing() {
    let (db, app, token) = setup().await;

    let (status, _) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/site_parameters/{PARAM_S1_TEMP_ID}/declare_sd_estimator"),
        &json!({ "estimator": "sample" }),
        &token,
    )
    .await;
    assert_eq!(status, 200);

    let (status, cleared) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/site_parameters/{PARAM_S1_TEMP_ID}/declare_sd_estimator"),
        &json!({ "estimator": null }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "clear: {cleared}");
    assert_eq!(cleared["previous"], json!("sample"));
    assert_eq!(cleared["estimator"], json!(null));
    assert_eq!(
        cleared["samples_affected"], 0,
        "stored samples keep the divisor they were computed with: {cleared}"
    );
    assert!(cleared.get("job_id").is_none(), "{cleared}");
    assert_eq!(slot_estimator(&db).await, None);
}

#[tokio::test]
#[serial]
async fn an_unknown_estimator_is_refused() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/site_parameters/{PARAM_S1_TEMP_ID}/declare_sd_estimator"),
        &json!({ "estimator": "mode" }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains("mode")),
        "the refusal names the value: {body}"
    );
}
