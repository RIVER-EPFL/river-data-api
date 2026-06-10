//! Pagination on GET /api/sites/{id}/status_events: limit/offset/order apply to JSON,
//! `total` reflects the full match set, CSV/NDJSON stay full-range exports.

use serial_test::serial;

async fn exec(db: &sea_orm::DatabaseConnection, sql: &str) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn seed_five_events(db: &sea_orm::DatabaseConnection) {
    let site_id = crate::common::SITE1_ID;
    let param_id = crate::common::GLOBAL_PARAM_TEMP_ID;
    let stream_id = "00000000-0000-4000-d000-000000000001";

    let rows: Vec<String> = (1..=5)
        .map(|h| {
            format!(
                "('{stream_id}', '{site_id}', '{param_id}', '2025-01-15T0{h}:00:00Z', 'status_{h}')"
            )
        })
        .collect();
    exec(
        db,
        &format!(
            "INSERT INTO status_events (stream_id, site_id, parameter_id, time, value) VALUES {}",
            rows.join(", ")
        ),
    )
    .await;
}

const RANGE: &str = "start=2025-01-15T00:00:00Z&end=2025-01-16T00:00:00Z";

fn values_of(body: &serde_json::Value) -> Vec<String> {
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["value"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
#[serial]
async fn test_no_limit_returns_all_with_total() {
    let (db, app, token) = setup().await;
    seed_five_events(&db).await;

    let site_id = crate::common::SITE1_ID;
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site_id}/status_events?{RANGE}"),
        &token,
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["total"], 5);
    assert_eq!(
        values_of(&body),
        ["status_1", "status_2", "status_3", "status_4", "status_5"]
    );
}

#[tokio::test]
#[serial]
async fn test_limit_and_offset() {
    let (db, app, token) = setup().await;
    seed_five_events(&db).await;

    let site_id = crate::common::SITE1_ID;
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site_id}/status_events?{RANGE}&limit=2"),
        &token,
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["total"], 5);
    assert_eq!(values_of(&body), ["status_1", "status_2"]);

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site_id}/status_events?{RANGE}&limit=2&offset=4"),
        &token,
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["total"], 5);
    assert_eq!(values_of(&body), ["status_5"]);
}

#[tokio::test]
#[serial]
async fn test_order_desc() {
    let (db, app, token) = setup().await;
    seed_five_events(&db).await;

    let site_id = crate::common::SITE1_ID;
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site_id}/status_events?{RANGE}&order=desc&limit=2"),
        &token,
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["total"], 5);
    assert_eq!(values_of(&body), ["status_5", "status_4"]);
}

#[tokio::test]
#[serial]
async fn test_csv_ignores_limit() {
    let (db, app, token) = setup().await;
    seed_five_events(&db).await;

    let site_id = crate::common::SITE1_ID;
    let (status, body) = crate::common::get_with_token(
        &app,
        &format!("/api/sites/{site_id}/status_events?{RANGE}&limit=2&format=csv"),
        &token,
    )
    .await;

    assert_eq!(status, 200);
    let data_rows = body.lines().skip(1).filter(|l| !l.is_empty()).count();
    assert_eq!(data_rows, 5, "CSV export should ignore limit");
}

#[tokio::test]
#[serial]
async fn test_oversized_limit_clamped() {
    let (db, app, token) = setup().await;
    seed_five_events(&db).await;

    let site_id = crate::common::SITE1_ID;
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site_id}/status_events?{RANGE}&limit=5000"),
        &token,
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["total"], 5);
    assert_eq!(body["events"].as_array().unwrap().len(), 5);
}
