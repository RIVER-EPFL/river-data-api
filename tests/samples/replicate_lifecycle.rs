//! Grab replicate lifecycle through the API: a replicated grab is stored as replicate_index
//! 0..n-1 rows behind one sample, the site endpoint serves the sample mean as the point value,
//! include_sample_stats exposes the replicates, and flagging a replicate moves the served mean.
//!
//! Run with: cargo test --test samples

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

const GRAB_TIME: &str = "2025-01-20T10:00:00Z";

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

fn grab_payload() -> serde_json::Value {
    let readings: Vec<serde_json::Value> = [10.0, 20.0, 30.0]
        .iter()
        .map(|v| {
            serde_json::json!({
                "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                "value": v,
                "time": GRAB_TIME,
            })
        })
        .collect();
    serde_json::json!({
        "site_id": crate::common::SITE1_ID,
        "created_by": "test",
        "readings": readings,
    })
}

async fn fetch_temp_series(app: &axum::Router, token: &str, extra: &str) -> serde_json::Value {
    let uri = format!(
        "/api/sites/{}/readings?start=2025-01-20T00:00:00Z&end=2025-01-21T00:00:00Z\
         &parameter_ids={}&measurement_type=spot{extra}",
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
    );
    let (status, body) = crate::common::get_with_token(app, &uri, token).await;
    assert_eq!(status, 200, "readings fetch ({status}): {body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
#[serial]
async fn grab_replicates_form_sample_and_serve_mean() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &grab_payload(), &token)
            .await;
    assert_eq!(status, 200, "grab insert ({status}): {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(resp["inserted"], 3);
    assert_eq!(resp["samples_created"], 1);

    let indices = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT replicate_index FROM readings \
                 WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{GRAB_TIME}' \
                 ORDER BY replicate_index",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get::<i16>("", "replicate_index").unwrap())
        .collect::<Vec<_>>();
    assert_eq!(indices, vec![0, 1, 2]);

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM samples \
                 WHERE site_id = '{}' AND parameter_id = '{}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        )
        .await,
        1
    );

    let series = fetch_temp_series(&app, &token, "").await;
    let values = series["parameters"][0]["values"].as_array().unwrap();
    assert_eq!(values.len(), 1, "one point per replicate group: {series}");
    assert!(
        (values[0].as_f64().unwrap() - 20.0).abs() < 1e-9,
        "served value is the sample mean: {values:?}"
    );

    let with_stats = fetch_temp_series(&app, &token, "&include_sample_stats=true").await;
    let stats = &with_stats["parameters"][0]["samples"][0];
    assert_eq!(stats["n"], 3, "sample stats attached: {with_stats}");
    assert_eq!(
        stats["replicates"].as_array().unwrap().len(),
        3,
        "all replicates listed: {stats}"
    );

    let (status, body) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &serde_json::json!({
            "reason": "outlier",
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                "time": GRAB_TIME,
                "replicate_index": 2,
            }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "flag one replicate ({status}): {body}");
    let flagged: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(flagged["updated"], 1, "only the named replicate is flagged");

    let series = fetch_temp_series(&app, &token, "").await;
    let values = series["parameters"][0]["values"].as_array().unwrap();
    assert!(
        (values[0].as_f64().unwrap() - 15.0).abs() < 1e-9,
        "flagging a replicate moves the served mean: {values:?}"
    );
}

#[tokio::test]
#[serial]
async fn grab_repost_conflicts_then_replace_rewrites_the_group() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, _body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &grab_payload(), &token)
            .await;
    assert_eq!(status, 200);

    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &grab_payload(), &token)
            .await;
    assert_eq!(status, 409, "bare re-post is refused ({status}): {body}");
    let conflict: serde_json::Value = serde_json::from_str(&body).unwrap();
    let groups = conflict["detail"].as_array().unwrap();
    assert_eq!(
        groups.len(),
        1,
        "the refusal names the stored group: {conflict}"
    );
    assert_eq!(
        groups[0]["replicates"].as_array().unwrap().len(),
        3,
        "with every stored replicate: {conflict}"
    );

    let mut replace = grab_payload();
    replace["mode"] = serde_json::json!("replace");
    replace["readings"] = serde_json::json!([{
        "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
        "value": 40.0,
        "time": GRAB_TIME,
    }, {
        "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
        "value": 60.0,
        "time": GRAB_TIME,
    }]);
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &replace, &token).await;
    assert_eq!(status, 200, "replace ({status}): {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        resp["replaced"], 3,
        "the whole stored set is removed: {resp}"
    );
    assert_eq!(resp["inserted"], 2, "and the new set stored: {resp}");

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM samples \
                 WHERE site_id = '{}' AND parameter_id = '{}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        )
        .await,
        1,
        "no duplicate sample accumulates on replace"
    );

    let series = fetch_temp_series(&app, &token, "").await;
    let values = series["parameters"][0]["values"].as_array().unwrap();
    assert_eq!(values.len(), 1);
    assert!(
        (values[0].as_f64().unwrap() - 50.0).abs() < 1e-9,
        "served mean covers only the replacement set: {values:?}"
    );
}

#[tokio::test]
#[serial]
async fn flagging_replicate_zero_keeps_the_grab_served() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, _body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &grab_payload(), &token)
            .await;
    assert_eq!(status, 200);

    let (status, body) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &serde_json::json!({
            "reason": "contaminated cuvette",
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                "time": GRAB_TIME,
                "replicate_index": 0,
            }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "flag replicate 0 ({status}): {body}");

    let series = fetch_temp_series(&app, &token, "&include_flagged=false").await;
    let values = series["parameters"][0]["values"].as_array().unwrap();
    assert_eq!(
        values.len(),
        1,
        "the grab stays served without its replicate 0: {series}"
    );
    assert!(
        (values[0].as_f64().unwrap() - 25.0).abs() < 1e-9,
        "served value is the mean over the surviving replicates: {values:?}"
    );

    for idx in [1, 2] {
        let (status, _body) = crate::common::patch_json_with_token(
            &app,
            "/api/readings/flag",
            &serde_json::json!({
                "reason": "contaminated cuvette",
                "readings": [{
                    "site_id": crate::common::SITE1_ID,
                    "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                    "time": GRAB_TIME,
                    "replicate_index": idx,
                }],
            }),
            &token,
        )
        .await;
        assert_eq!(status, 200);
    }

    let series = fetch_temp_series(&app, &token, "&include_flagged=false").await;
    let empty = series["parameters"][0]["values"]
        .as_array()
        .map_or(true, Vec::is_empty);
    assert!(
        empty,
        "a fully flagged grab disappears from the unflagged served set: {series}"
    );
}

#[tokio::test]
#[serial]
async fn duplicate_explicit_replicate_indices_are_refused() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let mut payload = grab_payload();
    payload["readings"] = serde_json::json!([{
        "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
        "value": 10.0,
        "time": GRAB_TIME,
        "replicate_index": 1,
    }, {
        "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
        "value": 20.0,
        "time": GRAB_TIME,
        "replicate_index": 1,
    }]);
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &payload, &token).await;
    assert_eq!(
        status, 409,
        "a duplicate index is refused ({status}): {body}"
    );

    payload["readings"][1]["replicate_index"] = serde_json::Value::Null;
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &payload, &token).await;
    assert_eq!(
        status, 400,
        "mixing explicit and automatic indices is refused ({status}): {body}"
    );

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM readings \
                 WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{GRAB_TIME}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        )
        .await,
        0,
        "a refused request stores nothing"
    );
}
