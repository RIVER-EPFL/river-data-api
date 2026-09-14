//! Enrollment credentials are listed only to a Keycloak administrator.
//!
//! A credential bootstraps a full-permission sync session token, so mint, revoke and the listing
//! sit behind the same gate as the `sync_service_credentials` CRUD surface. No API token passes,
//! whatever its permissions, and not even a sync session token, which would otherwise be able to
//! enumerate its own siblings. The administrator arm needs Keycloak and is covered by the
//! permission-matrix suite.
//!
//! Run: cargo test --test sync -- --test-threads=1

use serial_test::serial;

#[tokio::test]
#[serial]
async fn no_api_token_can_list_enrollment_credentials() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::seed_sync_credentials(&db, "svc_gate", "gate-secret", "test").await;
    let app = crate::common::build_test_app(db.clone());

    let full = crate::common::seed_token_full(&db).await;
    let metadata_only = crate::common::seed_token_read_metadata_only(&db).await;
    let (sync_session, _service_id) = crate::common::seed_sync_session_token(&db).await;

    for (label, token) in [
        ("full-permissions token", full),
        ("read_metadata token", metadata_only),
        ("sync session token", sync_session),
    ] {
        let (status, body) =
            crate::common::get_with_token(&app, "/api/sync_service_credentials", &token).await;
        assert_eq!(
            status, 403,
            "{label} must not list enrollment credentials ({status}): {body}"
        );
        assert!(
            !body.contains("svc_gate"),
            "{label} sees no client_id in the refusal: {body}"
        );
    }
}

/// Q75: services, commands and events have one URL each, the CRUD route, and it is
/// administrator-only. A read_metadata token reached them through `/api/sync/*` until the
/// hand-written listings went; neither URL admits it now.
#[tokio::test]
#[serial]
async fn a_metadata_token_reads_no_sync_service_command_or_event() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let token = crate::common::seed_token_read_metadata_only(&db).await;

    // `/sync/events` keeps its POST, the control plane's cycle report, so reading it is a 405
    // where the other two are gone outright.
    for (path, expected) in [
        ("/api/sync/services", 404),
        ("/api/sync/commands", 404),
        ("/api/sync/events", 405),
    ] {
        let (status, body) = crate::common::get_with_token(&app, path, &token).await;
        assert_eq!(status, expected, "GET {path} ({status}): {body}");
    }
    for path in [
        "/api/sync_services",
        "/api/sync_commands",
        "/api/sync_events",
    ] {
        let (status, body) = crate::common::get_with_token(&app, path, &token).await;
        assert_eq!(
            status, 403,
            "{path} is administrator-only ({status}): {body}"
        );
    }
}
