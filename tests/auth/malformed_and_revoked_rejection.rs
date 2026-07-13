//! Auth edge cases: malformed headers, race conditions, revocation timing, bypass attempts.
//!
//! Goal: catch class-of-bugs we don't think to add explicit tests for. Each case here is
//! either (a) a pre-existing security property the unified tier inherits and must not
//! regress, or (b) a property newly enforced by `require_admin` that needs explicit
//! coverage.


use chrono::{Duration, Utc};
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

#[tokio::test]
#[serial]
async fn malformed_authorization_headers_all_return_401() {
    let (_db, app) = setup().await;

    let cases: &[(&str, &str)] = &[
        ("missing scheme", "abcdef123"),
        ("not bearer scheme", "Basic dXNlcjpwYXNz"),
        ("not bearer scheme 2", "Digest realm=foo"),
        ("empty bearer", "Bearer "),
        ("bearer with only spaces", "Bearer    "),
        ("looks-like-jwt invalid signature", "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.invalidsig"),
        ("random opaque token", "Bearer not-a-real-token-12345"),
    ];

    for (label, header) in cases {
        let (status, _body) = crate::common::get_with_auth_header(&app, "/api/projects", header).await;
        assert_eq!(status, 401, "[{label}] expected 401, got {status}");
    }
}

#[tokio::test]
#[serial]
async fn inactive_token_returns_401_even_for_endpoints_it_was_authorized_for() {
    let (db, app) = setup().await;

    let tok = crate::common::seed_inactive_api_token(&db, crate::common::full_permissions()).await;

    let endpoints = [
        "/api/projects",
        "/api/sites",
        "/api/search?q=test",
    ];
    for path in endpoints {
        let (status, _) = crate::common::get_with_token(&app, path, &tok).await;
        assert_eq!(status, 401, "inactive token on {path} must be 401, got {status}");
    }
}

#[tokio::test]
#[serial]
async fn expired_token_returns_401_for_every_endpoint() {
    let (db, app) = setup().await;

    let tok = crate::common::seed_api_token_with_expiry(
        &db,
        crate::common::full_permissions(),
        None,
        Utc::now() - Duration::hours(1),
    )
    .await;

    let endpoints = [
        "/api/projects",
        "/api/search?q=foo",
        "/api/alarms/summary",
    ];
    for path in endpoints {
        let (status, _) = crate::common::get_with_token(&app, path, &tok).await;
        assert_eq!(status, 401, "expired token on {path} must be 401, got {status}");
    }
}

#[tokio::test]
#[serial]
async fn permission_json_with_missing_fields_uses_serde_defaults() {
    let (db, app) = setup().await;

    // Empty `{}` — serde defaults: read_metadata=true, read_data=true, write_*=false.
    let tok_empty = crate::common::seed_api_token(&db, serde_json::json!({}), None).await;
    let (status, _) = crate::common::get_with_token(&app, "/api/projects", &tok_empty).await;
    assert_eq!(status, 200, "empty permissions should default to read access, got {status}");

    let (write_status, _) = crate::common::post_json_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({}),
        &tok_empty,
    )
    .await;
    assert_eq!(write_status, 403, "empty permissions should default to NO write_metadata");
}

#[tokio::test]
#[serial]
async fn permission_json_with_unknown_keys_is_tolerated() {
    let (db, app) = setup().await;

    let tok = crate::common::seed_api_token(
        &db,
        serde_json::json!({
            "read_metadata": true,
            "read_data": true,
            "write_metadata": false,
            "write_data": false,
            "future_admin_flag": true,
            "another_unknown": "ignored"
        }),
        None,
    )
    .await;

    let (read_status, _) = crate::common::get_with_token(&app, "/api/projects", &tok).await;
    assert_eq!(read_status, 200, "read should succeed with known scopes");

    // The new unknown field must NOT escalate to admin access — proves the schema is closed.
    let (admin_status, _) = crate::common::get_with_token(&app, "/api/tokens", &tok).await;
    assert_eq!(
        admin_status, 403,
        "an unknown 'future_admin_flag' field must not grant admin access"
    );
}

#[tokio::test]
#[serial]
async fn permission_json_null_uses_defaults() {
    let (db, app) = setup().await;

    // Insert directly with null permissions JSON to bypass the helper.
    use river_db::routes::private::api_tokens::service::mint_api_token;
    let minted = mint_api_token();
    let raw = minted.raw_token;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO api_tokens (id, name, token_hash, token_prefix, permissions, is_active) \
             VALUES (gen_random_uuid(), 'null-perms', '{}', '{}', 'null'::jsonb, true)",
            minted.token_hash, minted.token_prefix
        ),
    )
    .await;

    let (status, _) = crate::common::get_with_token(&app, "/api/projects", &raw).await;
    // The from_json fallback returns the default TokenPermissions — reads on, writes off.
    assert_eq!(status, 200, "null permissions should fall back to defaults, got {status}");
}

#[tokio::test]
#[serial]
async fn revoked_sync_session_token_rejected_on_next_use() {
    let (db, app) = setup().await;

    let (tok, _service_id) = crate::common::seed_sync_session_token(&db).await;

    let (ok_status, _) = crate::common::get_with_token(&app, "/api/search?q=site", &tok).await;
    assert_eq!(ok_status, 200, "fresh sync session token should work, got {ok_status}");

    // Manually expire by setting expires_at to the past — emulates revoke_credential.
    crate::common::db::exec(
        &db,
        "UPDATE sync_service_tokens SET expires_at = now() - interval '1 hour'",
    )
    .await;

    let (rev_status, _) = crate::common::get_with_token(&app, "/api/search?q=site", &tok).await;
    assert_eq!(rev_status, 401, "expired sync session must be rejected, got {rev_status}");
}

#[tokio::test]
#[serial]
async fn require_admin_blocks_every_bypass_vector() {
    let (db, app) = setup().await;

    // Every "I might still be able to reach admin" token shape.
    let attempts: Vec<(&str, String)> = vec![
        (
            "full-permissions API token",
            crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await,
        ),
        (
            "project-scoped full token",
            crate::common::seed_api_token(
                &db,
                crate::common::full_permissions(),
                Some(crate::common::fixtures::PROJECT_ID),
            )
            .await,
        ),
        (
            "sync session token (auto-granted full perms)",
            crate::common::seed_sync_session_token(&db).await.0,
        ),
    ];

    let admin_routes = [
        ("GET", "/api/tokens"),
        ("POST", "/api/tokens"),
        ("GET", "/api/sync_service_credentials"),
        ("POST", "/api/sync/credentials"),
        ("POST", "/api/sync/credentials/00000000-0000-0000-0000-000000000000/revoke"),
    ];

    for (label, tok) in &attempts {
        for (method, path) in &admin_routes {
            let status = match *method {
                "GET" => crate::common::get_with_token(&app, path, tok).await.0,
                "POST" => {
                    crate::common::post_json_with_token(&app, path, &serde_json::json!({}), tok)
                        .await
                        .0
                }
                _ => unreachable!(),
            };
            assert_eq!(
                status, 403,
                "[{label}] {method} {path} expected 403, got {status}"
            );
        }
    }
}

#[tokio::test]
#[serial]
async fn anonymous_returns_401_not_403_on_admin_routes() {
    // Distinction matters: 401 = not authenticated (try again with auth),
    // 403 = authenticated but forbidden. Mixing them up is a UX issue.
    let (_db, app) = setup().await;

    let (status, _) = crate::common::get(&app, "/api/tokens").await;
    assert_eq!(status, 401, "anonymous on admin route should be 401, got {status}");

    let (status, _) = crate::common::get(&app, "/api/sync_service_credentials").await;
    assert_eq!(status, 401, "anonymous on admin route should be 401, got {status}");
}
