//! `created_by` names who made a row, so it is the authenticated caller's label on every CRUD
//! entity that carries one: a body naming someone else on create is not stored, and an update
//! naming it is refused.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::{Value, json};
use serial_test::serial;

use crate::common::keycloak as kc;
use crate::common::{
    FIXTURE_SENSOR_ID, GLOBAL_PARAM_TEMP_ID, SITE1_ID, get_json_with_token,
    post_json_parse_with_token, put_json_with_token, seeded_app,
};

const FOREIGN_AUTHOR: &str = "sync:cnet";

async fn token_caller(db: &DatabaseConnection) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT 'token:' || id::text AS caller FROM api_tokens",
    ))
    .await
    .unwrap()
    .expect("the seeded token")
    .try_get("", "caller")
    .unwrap()
}

/// Create one row naming a foreign author, returning its id after checking the stored author.
async fn create_as_caller(
    app: &axum::Router,
    token: &str,
    entity: &str,
    mut body: Value,
    caller: &str,
) -> String {
    body["created_by"] = json!(FOREIGN_AUTHOR);
    let (status, created) =
        post_json_parse_with_token(app, &format!("/api/{entity}"), &body, token).await;
    assert!(
        (200..300).contains(&status),
        "creating a {entity} row succeeds ({status}): {created}"
    );
    assert_eq!(
        created["created_by"].as_str(),
        Some(caller),
        "{entity}: the author is the caller, not the one the body named: {created}"
    );
    created["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{entity} row carries an id: {created}"))
        .to_string()
}

/// An update naming the author is refused and the stored author stands.
async fn reattribution_is_refused(
    app: &axum::Router,
    token: &str,
    entity: &str,
    id: &str,
    caller: &str,
) {
    let uri = format!("/api/{entity}/{id}");
    let (status, body) =
        put_json_with_token(app, &uri, &json!({ "created_by": "someone else" }), token).await;
    assert_eq!(
        status, 400,
        "{entity}: naming the author on update is refused: {body}"
    );
    let (status, served) = get_json_with_token(app, &uri, token).await;
    assert_eq!(status, 200, "{entity} still readable: {served}");
    assert_eq!(
        served["created_by"].as_str(),
        Some(caller),
        "{entity}: the stored author is unchanged: {served}"
    );
}

#[tokio::test]
#[serial]
async fn field_entities_store_the_caller_as_author_and_refuse_reattribution() {
    let fx = seeded_app().await;
    let caller = token_caller(&fx.db).await;

    let rows = [
        (
            "standard_curves",
            json!({ "sensor_id": FIXTURE_SENSOR_ID, "name": "Plate A", "slope": 2.0, "intercept": 1.0 }),
        ),
        (
            "collection_events",
            json!({ "site_id": SITE1_ID, "collected_at": "2025-03-01T09:00:00Z" }),
        ),
        (
            "annotations",
            json!({
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "start_time": "2025-03-01T00:00:00Z",
                "end_time": "2025-03-02T00:00:00Z",
                "text": "probe cleaned",
                "category": "maintenance",
            }),
        ),
        (
            "notes",
            json!({ "site_id": SITE1_ID, "text": "gate locked" }),
        ),
    ];
    for (entity, body) in rows {
        let id = create_as_caller(&fx.app, &fx.token, entity, body, &caller).await;
        reattribution_is_refused(&fx.app, &fx.token, entity, &id, &caller).await;
    }

    let (status, created) = post_json_parse_with_token(
        &fx.app,
        "/api/notes",
        &json!({ "site_id": SITE1_ID, "text": "no author named" }),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "{created}");
    assert_eq!(
        created["created_by"].as_str(),
        Some(caller.as_str()),
        "a row whose body names nobody is still attributed to the caller: {created}"
    );
    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/notes/{}", created["id"].as_str().unwrap()),
        &json!({ "text": "gate unlocked" }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "an update that leaves the author alone still succeeds: {body}"
    );
}

#[tokio::test]
#[serial]
async fn admin_entities_store_the_caller_as_author_and_refuse_reattribution() {
    if !crate::common::profile::Service::Keycloak
        .require("admin_entities_store_the_caller_as_author_and_refuse_reattribution")
        .await
    {
        return;
    }
    let fx = seeded_app().await;
    let app = kc::build_test_app_with_keycloak(fx.db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;
    let (status, me) = crate::common::get_json_with_token(&app, "/api/me", &jwt).await;
    assert_eq!(status, 200, "{me}");
    let caller = me["email"]
        .as_str()
        .or_else(|| me["sub"].as_str())
        .expect("the caller has a label")
        .to_string();

    let rows = [
        (
            "tokens",
            json!({ "name": "author test", "permissions": crate::common::full_permissions() }),
        ),
        (
            "pairing_plans",
            json!({ "source_system": "cnet", "summary": {}, "entries": [] }),
        ),
    ];
    for (entity, body) in rows {
        let id = create_as_caller(&app, &jwt, entity, body, &caller).await;
        reattribution_is_refused(&app, &jwt, entity, &id, &caller).await;
    }
}
