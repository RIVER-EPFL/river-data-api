//! Sync control plane over the in-process HTTP surface: enrollment, heartbeat, the command
//! issue→deliver→update lifecycle, sync-event reporting, service revocation, and health derivation.
//! `SyncState` is implemented for `AppState`, so every handler runs against the test DB with no live
//! infra. Auth: enroll is unauthenticated; heartbeat/command-update/event endpoints take a sync
//! session token; the admin issue-command/revoke/list endpoints take an API token.
//!
//! Out of scope here: `POST /api/sync/credentials` and `/credentials/{id}/revoke` are `require_admin`
//! (Keycloak Administrator only — no API token can pass, and the harness has no Keycloak), so
//! credential minting is exercised via the `seed_sync_credentials` helper and the
//! `revoke_service` path instead. The Keycloak-gated routes live in `e2e_keycloak_test.rs`.
//!
//! Run: cargo test --test sync -- --test-threads=1


use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(DatabaseBackend::Postgres, sql.to_string()))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<i64>("", "c").expect("c")
}

#[tokio::test]
#[serial]
async fn enroll_with_seeded_credentials_returns_session_token() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_sync_credentials(&db, "svc_test_client", "super-secret", "test").await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/enroll",
        &serde_json::json!({
            "client_id": "svc_test_client",
            "client_secret": "super-secret",
            "instance_id": "inst-1"
        }),
        "",
    )
    .await;
    assert_eq!(status, 200, "enroll ({status}): {body}");

    let service_id = body["service_id"].as_str().expect("service_id");
    assert!(!service_id.is_empty(), "service_id present: {body}");
    assert!(
        !body["session_token"].as_str().unwrap_or("").is_empty(),
        "session_token present: {body}"
    );

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM sync_services WHERE id = '{service_id}' \
                 AND service_type = 'test' AND instance_id = 'inst-1' AND status = 'starting'"
            ),
        )
        .await,
        1,
        "a starting sync_services row was created"
    );
    assert!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM sync_service_tokens WHERE service_id = '{service_id}'"),
        )
        .await
            >= 1,
        "a session token row was created"
    );
}

#[tokio::test]
#[serial]
async fn enroll_with_bad_credentials_is_rejected() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_sync_credentials(&db, "svc_x", "right-secret", "test").await;
    let app = crate::common::build_test_app(db.clone());

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/enroll",
        &serde_json::json!({"client_id": "svc_nope", "client_secret": "x", "instance_id": "i"}),
        "",
    )
    .await;
    assert_eq!(status, 401, "unknown client_id rejected");

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/enroll",
        &serde_json::json!({"client_id": "svc_x", "client_secret": "wrong", "instance_id": "i"}),
        "",
    )
    .await;
    assert_eq!(status, 401, "wrong client_secret rejected");

    assert_eq!(
        count(&db, "SELECT count(*) AS c FROM sync_services").await,
        0,
        "no service row created for rejected enrollments"
    );
}

#[tokio::test]
#[serial]
async fn heartbeat_updates_service_and_returns_no_pending_commands() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (token, service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "idle", "current_operation": null}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "heartbeat ({status}): {body}");
    assert!(
        body["pending_commands"].as_array().expect("array").is_empty(),
        "no commands queued: {body}"
    );
    assert!(
        !body["session_token"].as_str().unwrap_or("").is_empty(),
        "heartbeat returns a session token: {body}"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM sync_services WHERE id = '{service_id}' \
                 AND status = 'idle' AND last_heartbeat IS NOT NULL"
            ),
        )
        .await,
        1,
        "heartbeat recorded status + last_heartbeat"
    );

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "frobnicate"}),
        &token,
    )
    .await;
    assert_eq!(status, 400, "invalid status rejected");

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "idle"}),
        "",
    )
    .await;
    assert_eq!(status, 401, "missing session token rejected");
}

#[tokio::test]
#[serial]
async fn command_lifecycle_issue_deliver_acknowledge_complete() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (token, service_id) = crate::common::seed_sync_session_token(&db).await;
    let admin = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, cmd) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/sync/services/{service_id}/commands"),
        &serde_json::json!({"command": "trigger_sync", "payload": null}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "issue command ({status}): {cmd}");
    assert_eq!(cmd["status"], "pending");
    assert_eq!(cmd["command"], "trigger_sync");
    let command_id = cmd["id"].as_str().expect("command id").to_string();

    for command in ["trigger_full_sync", "pause", "resume"] {
        let (status, _) = crate::common::post_json_with_token(
            &app,
            &format!("/api/sync/services/{service_id}/commands"),
            &serde_json::json!({"command": command}),
            &admin,
        )
        .await;
        assert_eq!(status, 200, "issue {command}");
    }

    let (status, _) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/services/{service_id}/commands"),
        &serde_json::json!({"command": "full_sync"}),
        &admin,
    )
    .await;
    assert_eq!(status, 400, "invalid command name rejected (it is trigger_full_sync)");

    let (status, hb) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "running"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "heartbeat ({status}): {hb}");
    let pending = hb["pending_commands"].as_array().expect("array");
    assert!(
        pending.iter().any(|c| c["id"] == serde_json::json!(command_id)
            && c["command"] == "trigger_sync"),
        "trigger_sync delivered: {hb}"
    );

    let (status, _) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/commands/{command_id}"),
        &serde_json::json!({"status": "acknowledged", "result": null}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "acknowledge command");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM sync_commands WHERE id = '{command_id}' \
                 AND status = 'acknowledged' AND acknowledged_at IS NOT NULL"
            ),
        )
        .await,
        1,
        "command acknowledged with timestamp"
    );

    let (status, _) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/commands/{command_id}"),
        &serde_json::json!({"status": "completed", "result": {"readings": 5}}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "complete command");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM sync_commands WHERE id = '{command_id}' \
                 AND status = 'completed' AND completed_at IS NOT NULL"
            ),
        )
        .await,
        1,
        "command completed with timestamp"
    );

    let (status, _) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/commands/{command_id}"),
        &serde_json::json!({"status": "pending"}),
        &token,
    )
    .await;
    assert_eq!(status, 400, "invalid update status rejected");

    let (other_token, _other_service) = crate::common::seed_sync_session_token(&db).await;
    let (status, _) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/commands/{command_id}"),
        &serde_json::json!({"status": "acknowledged"}),
        &other_token,
    )
    .await;
    assert_eq!(status, 403, "another service cannot update this command");
}

#[tokio::test]
#[serial]
async fn sync_event_create_update_and_read_back() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (token, service_id) = crate::common::seed_sync_session_token(&db).await;
    let admin = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, ev) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/events",
        &serde_json::json!({
            "service_id": service_id, "command_id": null,
            "event_type": "manual", "status": "running"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create event ({status}): {ev}");
    assert_eq!(ev["status"], "running");
    let event_id = ev["id"].as_str().expect("event id").to_string();

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/events",
        &serde_json::json!({"service_id": service_id, "event_type": "nonsense"}),
        &token,
    )
    .await;
    assert_eq!(status, 400, "invalid event_type rejected");

    let bogus = uuid::Uuid::new_v4();
    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/events",
        &serde_json::json!({"service_id": bogus, "event_type": "manual"}),
        &token,
    )
    .await;
    assert_eq!(status, 403, "service_id mismatch rejected");

    let (status, _) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/events/{event_id}"),
        &serde_json::json!({
            "status": "completed", "readings_synced": 42,
            "status_events_synced": 3, "duration_ms": 1200, "errors": null, "log": null
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "update event");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM sync_events WHERE id = '{event_id}' \
                 AND status = 'completed' AND readings_synced = 42 AND completed_at IS NOT NULL"
            ),
        )
        .await,
        1,
        "event completed with metrics"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM sync_services WHERE id = '{service_id}' \
                 AND last_sync_completed_at IS NOT NULL"
            ),
        )
        .await,
        1,
        "successful event stamped last_sync_completed_at on the service"
    );

    let (status, events) = crate::common::get_json_with_token(&app, "/api/sync/events", &admin).await;
    assert_eq!(status, 200, "list events ({status}): {events}");
    let found = events
        .as_array()
        .expect("array")
        .iter()
        .find(|e| e["id"] == serde_json::json!(event_id))
        .expect("event in list");
    assert_eq!(found["readings_synced"], 42);
    assert_eq!(found["status"], "completed");
}

#[tokio::test]
#[serial]
async fn revoke_service_kills_active_session() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (token, service_id) = crate::common::seed_sync_session_token(&db).await;
    let admin = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "idle"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "session is live before revoke");

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/sync/services/{service_id}/revoke"),
        &serde_json::json!({}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "revoke ({status}): {body}");
    assert_eq!(body["revoked"], true);
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM sync_service_tokens WHERE service_id = '{service_id}'"),
        )
        .await,
        0,
        "session tokens deleted on revoke"
    );

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "idle"}),
        &token,
    )
    .await;
    assert_eq!(status, 401, "the revoked session no longer authenticates");
}

#[tokio::test]
#[serial]
async fn health_state_derived_from_heartbeat_recency() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let read_token = crate::common::seed_token_read_metadata_only(&db).await;

    let unknown = uuid::Uuid::new_v4();
    let healthy = uuid::Uuid::new_v4();
    let warning = uuid::Uuid::new_v4();
    let stale = uuid::Uuid::new_v4();
    let rows = [
        (unknown, "NULL"),
        (healthy, "now() - interval '30 seconds'"),
        (warning, "now() - interval '200 seconds'"),
        (stale, "now() - interval '600 seconds'"),
    ];
    for (id, hb) in rows {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO sync_services (id, service_type, instance_id, status, last_heartbeat, created_at, updated_at) \
                 VALUES ('{id}', 'test', '{id}', 'idle', {hb}, now(), now())"
            ),
        )
        .await;
    }

    let app = crate::common::build_test_app(db.clone());
    let (status, services) = crate::common::get_json_with_token(&app, "/api/sync/services", &read_token).await;
    assert_eq!(status, 200, "list services ({status}): {services}");
    let arr = services.as_array().expect("array");
    let health_of = |id: uuid::Uuid| -> String {
        arr.iter()
            .find(|s| s["id"] == serde_json::json!(id.to_string()))
            .unwrap_or_else(|| panic!("service {id} missing: {services}"))["health"]
            .as_str()
            .expect("health string")
            .to_string()
    };
    assert_eq!(health_of(unknown), "unknown");
    assert_eq!(health_of(healthy), "healthy");
    assert_eq!(health_of(warning), "warning");
    assert_eq!(health_of(stale), "stale");

    let (status, one) =
        crate::common::get_json_with_token(&app, &format!("/api/sync/services/{healthy}"), &read_token).await;
    assert_eq!(status, 200, "get service ({status}): {one}");
    assert_eq!(one["health"], "healthy");
}

#[tokio::test]
#[serial]
async fn heartbeat_for_another_service_is_forbidden() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let (_other_token, other_service) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": other_service, "status": "idle"}),
        &token,
    )
    .await;
    assert_eq!(status, 403, "cannot heartbeat as another service");
}

#[tokio::test]
#[serial]
async fn pause_persists_across_reenroll() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_sync_credentials(&db, "svc_pause_client", "super-secret", "test").await;
    let admin = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let enroll_body = serde_json::json!({
        "client_id": "svc_pause_client",
        "client_secret": "super-secret",
        "instance_id": "inst-pause"
    });
    let (status, body) =
        crate::common::post_json_parse_with_token(&app, "/api/sync/enroll", &enroll_body, "").await;
    assert_eq!(status, 200, "enroll ({status}): {body}");
    assert_eq!(body["paused"], false, "fresh service is not paused: {body}");
    let service_id = body["service_id"].as_str().expect("service_id").to_string();
    let token = body["session_token"].as_str().expect("token").to_string();

    let (status, _) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/services/{service_id}/commands"),
        &serde_json::json!({"command": "pause"}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "issue pause");
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM sync_services WHERE id = '{service_id}' AND paused"),
        )
        .await,
        1,
        "paused persisted at command issue time"
    );

    let (status, body) =
        crate::common::post_json_parse_with_token(&app, "/api/sync/enroll", &enroll_body, "").await;
    assert_eq!(status, 200, "re-enroll ({status}): {body}");
    assert_eq!(body["paused"], true, "pause survives re-enrollment: {body}");

    let (status, hb) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "paused"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "heartbeat ({status}): {hb}");
    assert_eq!(hb["paused"], true, "heartbeat reports pause: {hb}");

    let (status, _) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/services/{service_id}/commands"),
        &serde_json::json!({"command": "resume"}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "issue resume");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM sync_services WHERE id = '{service_id}' AND NOT paused"
            ),
        )
        .await,
        1,
        "resume clears the persisted pause"
    );
}

#[tokio::test]
#[serial]
async fn stale_running_sync_events_are_swept() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (_token, service_id) = crate::common::seed_sync_session_token(&db).await;

    for (age, label) in [("2 hours", "stale"), ("1 minute", "fresh")] {
        db.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "INSERT INTO sync_events (service_id, event_type, status, started_at) \
                 VALUES ('{service_id}', 'scheduled', 'running', NOW() - INTERVAL '{age}')"
            ),
        ))
        .await
        .unwrap_or_else(|e| panic!("insert {label} event: {e}"));
    }

    let closed = river_db::routes::private::reprocessing_jobs::jobs::sweep_stale_sync_events(&db, 3600)
        .await
        .expect("sweep");
    assert_eq!(closed, 1, "only the stale event is closed");

    assert_eq!(
        count(&db, "SELECT count(*) AS c FROM sync_events WHERE status = 'failed'").await,
        1,
        "stale event failed"
    );
    assert_eq!(
        count(&db, "SELECT count(*) AS c FROM sync_events WHERE status = 'running'").await,
        1,
        "fresh event untouched"
    );
}
