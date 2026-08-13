//! Access-gate tests: authentication alone is not membership. The RIVER realm is EPFL-federated
//! and auto-assigns `default-roles-river` to every login, so a valid JWT without an explicit
//! `riverdata-*` role must be rejected with a distinct 403 (`no_river_role`) on every route.
//!
//! Fixture users are provisioned through the dev Keycloak's admin API (idempotent), since the
//! realm import only ships `admin` + `user`. Auto-skips when Keycloak is unreachable.

use crate::common::keycloak::{
    build_test_app_with_keycloak, ensure_realm_user, get_keycloak_jwt, keycloak_reachable,
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
    build_test_app_with_keycloak(db).await
}

#[tokio::test]
#[serial]
async fn login_without_river_role_is_rejected_everywhere() {
    require_keycloak!();
    let app = seeded_app().await;
    ensure_realm_user("norole", "norole", &[]).await;
    let jwt = get_keycloak_jwt("norole", "norole").await;

    for uri in ["/api/projects", "/api/sites", "/api/search?q=Station"] {
        let (s, body) = crate::common::get_with_token(&app, uri, &jwt).await;
        assert_eq!(
            s, 403,
            "role-less JWT on {uri} must be 403, got {s}: {body}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        assert_eq!(
            parsed["error"], "no_river_role",
            "gate must return the distinct body on {uri}: {body}"
        );
    }

    let batch = serde_json::json!({"readings": []});
    let (s, body) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &batch, &jwt).await;
    assert_eq!(s, 403, "role-less JWT must not write data, got {s}: {body}");
}

#[tokio::test]
#[serial]
async fn admin_only_role_passes_the_gate() {
    // Regression for the removed `required_roles(vec![Role::User])` layer, which rejected any JWT
    // lacking the base role during validation, locking out a pure `riverdata-admin` account.
    require_keycloak!();
    let app = seeded_app().await;
    ensure_realm_user("adminonly", "adminonly", &["riverdata-admin"]).await;
    let jwt = get_keycloak_jwt("adminonly", "adminonly").await;

    let (s, body) = crate::common::get_with_token(&app, "/api/tokens", &jwt).await;
    assert_eq!(s, 200, "riverdata-admin alone must pass the gate: {body}");
}

#[tokio::test]
#[serial]
async fn river_user_access_is_unchanged() {
    require_keycloak!();
    let app = seeded_app().await;
    let jwt = get_keycloak_jwt("user", "user").await;

    let (s, _) = crate::common::get_with_token(&app, "/api/projects", &jwt).await;
    assert_eq!(s, 200, "a River user keeps read access through the gate");
}
