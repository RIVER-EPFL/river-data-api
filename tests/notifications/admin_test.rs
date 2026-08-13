//! Admin notification oversight: the health probe endpoint, the one-off test-send, and the subscriber
//! roster. All are admin-only (`require_admin`). Requires the dev Keycloak for real JWTs; auto-skips.
//!
//! Run with the dev stack up: cargo test --test notifications -- --test-threads=1

use crate::common::keycloak::{build_test_app_with_keycloak, get_keycloak_jwt, keycloak_reachable};
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
    build_test_app_with_keycloak(db).await
}

#[tokio::test]
#[serial]
async fn health_is_admin_only_and_lists_channels() {
    require_keycloak!();
    let app = seeded_app().await;

    let admin = get_keycloak_jwt("admin", "admin").await;
    let (s, body) =
        crate::common::get_json_with_token(&app, "/api/notifications/health", &admin).await;
    assert_eq!(s, 200, "admin reads health");
    let channels = body["channels"].as_array().expect("channels array");
    let names: Vec<&str> = channels.iter().filter_map(|c| c["name"].as_str()).collect();
    assert!(
        names.contains(&"telegram") && names.contains(&"email"),
        "both channels reported"
    );
    // Test config has neither channel configured.
    for c in channels {
        assert_eq!(c["available"], false, "channel unavailable in test config");
    }

    let user = get_keycloak_jwt("user", "user").await;
    let (s, _) = crate::common::get_with_token(&app, "/api/notifications/health", &user).await;
    assert_eq!(s, 403, "a non-admin cannot read channel health");
}

#[tokio::test]
#[serial]
async fn test_send_rejects_unconfigured_channel() {
    require_keycloak!();
    let app = seeded_app().await;
    let admin = get_keycloak_jwt("admin", "admin").await;

    let (s, body) = crate::common::post_json_with_token(
        &app,
        "/api/notifications/test-send",
        &serde_json::json!({ "channel": "telegram", "recipient": "123456" }),
        &admin,
    )
    .await;
    assert_eq!(
        s, 400,
        "test send fails fast when the channel isn't configured: {body}"
    );
}

#[tokio::test]
#[serial]
async fn subscriber_roster_lists_opted_in_users() {
    require_keycloak!();
    let app = seeded_app().await;
    let user = get_keycloak_jwt("user", "user").await;
    let admin = get_keycloak_jwt("admin", "admin").await;

    // The user touching /me creates their subscriber row.
    let (s, _) = crate::common::get_json_with_token(&app, "/api/notifications/me", &user).await;
    assert_eq!(s, 200);

    let (s, body) =
        crate::common::get_json_with_token(&app, "/api/notifications/subscribers", &admin).await;
    assert_eq!(s, 200, "admin reads the roster");
    let roster = body.as_array().expect("roster array");
    assert!(
        !roster.is_empty(),
        "the opted-in user appears in the roster"
    );
    assert!(
        roster
            .iter()
            .all(|r| r["telegram_status"].is_string() && r["keycloak_sub"].is_string()),
        "each row carries sub + telegram_status"
    );

    // Roster is admin-only.
    let (s, _) = crate::common::get_with_token(&app, "/api/notifications/subscribers", &user).await;
    assert_eq!(s, 403, "a non-admin cannot read the roster");
}
