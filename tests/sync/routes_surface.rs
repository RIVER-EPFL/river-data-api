//! The sync URL surface, asserted as a table.
//!
//! Every path here is one the dashboard or a sync service calls by literal string, so a route
//! that moves or is renamed breaks a caller outside this repo. The assertion is deliberately
//! weak on status (auth and validation are covered elsewhere) and strong on existence: a 404 or
//! 405 means the route is gone.
//!
//! Run: cargo test --test sync -- --test-threads=1

use axum::body::Body;
use axum::Router;
use http_body_util::BodyExt;
use serial_test::serial;
use tower::ServiceExt;
use uuid::Uuid;

async fn call(app: &Router, method: &str, uri: &str, token: &str) -> u16 {
    let body = if method == "GET" {
        Body::empty()
    } else {
        Body::from("{}")
    };
    let req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body)
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let _ = response.into_body().collect().await;
    status
}

#[tokio::test]
#[serial]
async fn every_sync_route_the_dashboard_and_services_call_still_exists() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (_raw, service_id) = crate::common::seed_sync_session_token(&db).await;
    crate::common::seed_sync_credentials(&db, "svc_surface", "surface-secret", "test").await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let credential_id = {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT id::text AS v FROM sync_service_credentials WHERE client_id = 'svc_surface'"
                    .to_string(),
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get::<String>("", "v").unwrap()
    };
    let command_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();

    // Control plane, called by river-data-core's runner on behalf of every sync service.
    // Unauthenticated or session-token routes: a 401 still proves the route is mounted.
    let control = [
        ("POST", "/api/sync/enroll".to_string()),
        ("POST", "/api/sync/heartbeat".to_string()),
        ("PATCH", format!("/api/sync/commands/{command_id}")),
        ("POST", "/api/sync/events".to_string()),
        ("PATCH", format!("/api/sync/events/{event_id}")),
    ];

    // Operator surface, called by the dashboard (river-data-ui/src/lib/api/service.ts).
    let operator = [
        ("GET", "/api/sync/services".to_string()),
        ("GET", format!("/api/sync/services/{service_id}")),
        ("GET", "/api/sync/commands".to_string()),
        ("GET", "/api/sync/events".to_string()),
        ("GET", "/api/sync/credentials".to_string()),
        ("POST", format!("/api/sync/services/{service_id}/commands")),
        ("POST", format!("/api/sync/services/{service_id}/revoke")),
        ("POST", "/api/sync/credentials".to_string()),
        ("POST", format!("/api/sync/credentials/{credential_id}/revoke")),
    ];

    // Entity listings, read by the dashboard's system page.
    let crud = [
        ("GET", "/api/sync_services".to_string()),
        ("GET", "/api/sync_commands".to_string()),
        ("GET", "/api/sync_events".to_string()),
        ("GET", "/api/sync_service_credentials".to_string()),
    ];

    for (method, uri) in control.iter().chain(operator.iter()).chain(crud.iter()) {
        let status = call(&app, method, uri, &token).await;
        assert!(
            status != 404 && status != 405,
            "{method} {uri} returned {status}: the route is missing or takes a different method"
        );
    }
}
