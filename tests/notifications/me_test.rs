//! Self-service `/api/notifications/me`: a Keycloak user manages only their OWN preferences. Every
//! handler binds to the JWT `sub`, so a second user gets a separate, empty record, no cross-user
//! access. Requires the dev Keycloak for real JWTs; auto-skips when it's unreachable.
//!
//! Run with the dev stack up: cargo test --test notifications -- --test-threads=1

use crate::common::fixtures::{GLOBAL_PARAM_TURB_ID, PROJECT_ID, SITE1_ID};
use crate::common::keycloak::{
    build_test_app_with_keycloak, get_keycloak_jwt, grant_project, keycloak_reachable,
    keycloak_user_id,
};
use serial_test::serial;

macro_rules! require_keycloak {
    () => {
        if !keycloak_reachable().await {
            eprintln!("SKIP: keycloak unreachable (start the dev stack, or set TEST_KEYCLOAK_URL)");
            return;
        }
    };
}

async fn seeded_app() -> axum::Router {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    // `user` is river level; site-scoped subscriptions require a project grant
    let user_sub = keycloak_user_id("user").await;
    grant_project(&db, &user_sub, PROJECT_ID).await;
    build_test_app_with_keycloak(db).await
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("bad json: {e}\n{text}"))
}

#[tokio::test]
#[serial]
async fn me_defaults_then_self_scoped_changes() {
    require_keycloak!();
    let app = seeded_app().await;
    let user = get_keycloak_jwt("user", "user").await;

    let (s, body) = crate::common::get_json_with_token(&app, "/api/notifications/me", &user).await;
    assert_eq!(s, 200);
    assert_eq!(body["email_enabled"], false, "email off by default");
    assert_eq!(body["telegram_enabled"], true, "telegram on by default");
    assert_eq!(body["telegram"]["status"], "unlinked");
    assert_eq!(body["subscriptions"].as_array().unwrap().len(), 0);

    let (s, body) = crate::common::patch_json_with_token(
        &app,
        "/api/notifications/me",
        &serde_json::json!({ "telegram_enabled": false }),
        &user,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(json(&body)["telegram_enabled"], false);

    let (s, body) = crate::common::put_json_with_token(
        &app,
        "/api/notifications/me/subscriptions",
        &serde_json::json!({ "subscriptions": [{ "site_id": SITE1_ID, "enabled": false }] }),
        &user,
    )
    .await;
    assert_eq!(s, 200, "body: {body}");
    let subs = json(&body);
    let subs = subs["subscriptions"].as_array().unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0]["site_id"], SITE1_ID);
    assert_eq!(subs[0]["enabled"], false);

    // A different user sees a separate, empty record, no cross-user leakage of prefs.
    let admin = get_keycloak_jwt("admin", "admin").await;
    let (s, body) = crate::common::get_json_with_token(&app, "/api/notifications/me", &admin).await;
    assert_eq!(s, 200);
    assert_eq!(body["subscriptions"].as_array().unwrap().len(), 0);
    assert_eq!(
        body["telegram_enabled"], true,
        "admin keeps its own default"
    );
}

#[tokio::test]
#[serial]
async fn me_link_then_unlink_telegram() {
    require_keycloak!();
    let app = seeded_app().await;
    let user = get_keycloak_jwt("user", "user").await;

    let (s, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/notifications/me/link_code",
        &serde_json::json!({}),
        &user,
    )
    .await;
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(
        body["code"].as_str().map(str::len),
        Some(8),
        "8-char code minted"
    );

    let (_s, body) = crate::common::get_json_with_token(&app, "/api/notifications/me", &user).await;
    assert_eq!(
        body["telegram"]["status"], "pending",
        "pending after minting a code"
    );

    let (s, _) =
        crate::common::delete_with_token(&app, "/api/notifications/me/telegram", &user).await;
    assert_eq!(s, 204);
    let (_s, body) = crate::common::get_json_with_token(&app, "/api/notifications/me", &user).await;
    assert_eq!(
        body["telegram"]["status"], "unlinked",
        "unlinked after delete"
    );
}

#[tokio::test]
#[serial]
async fn me_rejects_parameter_only_subscription() {
    require_keycloak!();
    let app = seeded_app().await;
    let user = get_keycloak_jwt("user", "user").await;

    let (s, _) = crate::common::put_json_with_token(
        &app,
        "/api/notifications/me/subscriptions",
        &serde_json::json!({ "subscriptions": [{ "parameter_id": GLOBAL_PARAM_TURB_ID, "enabled": true }] }),
        &user,
    )
    .await;
    assert_eq!(s, 400, "a parameter-only scope (no site) must be rejected");
}
