//! `needs_review` on a site parameter: set by tool-save provisioning, served by the CRUD read,
//! cleared by a PUT. Run with: cargo test --test site_parameters needs_review

use serde_json::json;
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn get(app: &axum::Router, token: &str, id: &str) -> serde_json::Value {
    let (status, text) =
        crate::common::get_with_token(app, &format!("/api/site_parameters/{id}"), token).await;
    assert_eq!(status, 200, "get ({status}): {text}");
    serde_json::from_str(&text).unwrap()
}

async fn provisioned_slot(db: &sea_orm::DatabaseConnection) -> String {
    use sea_orm::ConnectionTrait;
    let id = uuid::Uuid::new_v4();
    db.execute_raw(sea_orm::Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, is_active, \
         is_public, needs_review, created_at)
         VALUES ($1, $2, $3, 'Depth', '', TRUE, FALSE, TRUE, NOW())",
        [
            id.into(),
            uuid::Uuid::parse_str(crate::common::SITE2_ID)
                .unwrap()
                .into(),
            uuid::Uuid::parse_str(crate::common::GLOBAL_PARAM_DEPTH_ID)
                .unwrap()
                .into(),
        ],
    ))
    .await
    .unwrap();
    id.to_string()
}

#[tokio::test]
#[serial]
async fn a_provisioned_slot_is_served_as_needing_review_until_confirmed() {
    let (db, app, token) = setup().await;
    let id = provisioned_slot(&db).await;

    assert_eq!(get(&app, &token, &id).await["needs_review"], json!(true));

    let (status, text) = crate::common::put_json_with_token(
        &app,
        &format!("/api/site_parameters/{id}"),
        &json!({ "needs_review": false }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "confirm ({status}): {text}");
    assert_eq!(get(&app, &token, &id).await["needs_review"], json!(false));
}

#[tokio::test]
#[serial]
async fn a_slot_created_by_hand_does_not_need_review() {
    let (_db, app, token) = setup().await;
    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &json!({
            "site_id": crate::common::SITE2_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {text}");
    let created: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(created["needs_review"], json!(false), "{created}");
    assert_eq!(
        get(&app, &token, created["id"].as_str().unwrap()).await["needs_review"],
        json!(false)
    );
}

#[tokio::test]
#[serial]
async fn the_review_flag_is_filterable() {
    let (db, app, token) = setup().await;
    let id = provisioned_slot(&db).await;
    let (status, text) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/site_parameters?filter={}",
            crate::common::e2e::percent_encode(r#"{"needs_review":true}"#)
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "list ({status}): {text}");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    let ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(ids, vec![id.as_str()], "{text}");
}
