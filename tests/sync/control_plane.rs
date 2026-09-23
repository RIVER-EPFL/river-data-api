//! Sync control plane over the in-process HTTP surface: enrollment, heartbeat, the command
//! issue→deliver→update lifecycle, sync-event reporting, and service revocation.
//! The handlers take `AppState` directly, so every one runs against the test DB with no live
//! infra. Auth: enroll is unauthenticated; heartbeat/command-update/event endpoints take a sync
//! session token; the admin issue-command/revoke/list endpoints take an API token.
//!
//! Out of scope here: `POST /api/sync/credentials` and `/credentials/{id}/revoke` are `require_admin`
//! (Keycloak Administrator only, no API token can pass), so credential minting is exercised via
//! the `seed_sync_credentials` helper and the `revoke_service` path instead. The one
//! administrator route driven here is `PUT /api/sync_services/{id}`, under a profile covering
//! Keycloak.
//!
//! Run: cargo test --test sync -- --test-threads=1

use crudcrate::CRUDResource;
use river_db::routes::private::sync::models::events::SyncEvent;
use river_db::routes::private::sync::models::services::SyncServiceUpdate;
use river_db::routes::private::sync::models::services::{self, SyncService};
use river_db::routes::private::sync::service::SyncServiceOperations;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, EntityTrait, Statement};
use serial_test::serial;

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            sql.to_string(),
        ))
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
            &format!(
                "SELECT count(*) AS c FROM sync_service_tokens WHERE service_id = '{service_id}'"
            ),
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
        body["pending_commands"]
            .as_array()
            .expect("array")
            .is_empty(),
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
async fn the_sync_cadence_is_set_by_an_operator_and_carried_on_the_heartbeat() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (token, service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    // The cadence is set through the one URL the table has, `PUT /api/sync_services/{id}`,
    // which is Keycloak-administrator only; the update is driven here so the floor and the
    // heartbeat that carries it are asserted without Keycloak.
    let set_cadence = async |secs: Option<i32>| {
        crudcrate::CRUDOperations::update(
            &SyncServiceOperations,
            &db,
            service_id,
            SyncServiceUpdate {
                sync_interval_secs: Some(secs),
                service_type: None,
                instance_id: None,
                status: None,
                paused: None,
                current_operation: None,
                full_reassert_enabled: None,
                last_sync_completed_at: None,
            },
        )
        .await
    };

    let (status, hb) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "idle"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "heartbeat ({status}): {hb}");
    assert!(
        hb["sync_interval_secs"].is_null(),
        "an unset cadence leaves the service on its own configuration: {hb}"
    );

    let refused = set_cadence(Some(10))
        .await
        .expect_err("a cadence under the runner's floor is refused");
    assert!(
        refused.to_string().contains("at least 30"),
        "the refusal names the floor: {refused}"
    );

    let updated = set_cadence(Some(3600)).await.expect("set cadence");
    assert_eq!(updated.sync_interval_secs, Some(3600));

    let (_, hb) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "idle"}),
        &token,
    )
    .await;
    assert_eq!(
        hb["sync_interval_secs"], 3600,
        "the running service learns the cadence from its heartbeat: {hb}"
    );

    let cleared = set_cadence(None).await.expect("clear cadence");
    assert_eq!(cleared.sync_interval_secs, None);
    let (_, hb) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "idle"}),
        &token,
    )
    .await;
    assert!(
        hb["sync_interval_secs"].is_null(),
        "clearing returns the service to its own configuration: {hb}"
    );
}

async fn heartbeat_interval(
    app: &axum::Router,
    token: &str,
    service_id: uuid::Uuid,
) -> serde_json::Value {
    let (status, hb) = crate::common::post_json_parse_with_token(
        app,
        "/api/sync/heartbeat",
        &serde_json::json!({"service_id": service_id, "status": "idle"}),
        token,
    )
    .await;
    assert_eq!(status, 200, "heartbeat ({status}): {hb}");
    hb["sync_interval_secs"].clone()
}

#[tokio::test]
#[serial]
async fn an_administrator_sets_the_cadence_and_full_reassert_over_http() {
    if !crate::common::profile::Service::Keycloak
        .require("an_administrator_sets_the_cadence_and_full_reassert_over_http")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (token, service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::keycloak::build_test_app_with_keycloak(db.clone()).await;
    let admin = crate::common::keycloak::get_keycloak_jwt("admin", "admin").await;
    let uri = format!("/api/sync_services/{service_id}");

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &uri,
        &serde_json::json!({"sync_interval_secs": 10}),
        &admin,
    )
    .await;
    assert_eq!(status, 400, "a cadence under the floor is refused: {body}");
    assert!(
        body.contains("at least 30"),
        "the refusal names the floor: {body}"
    );

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &uri,
        &serde_json::json!({"sync_interval_secs": 3600}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "set cadence ({status}): {body}");
    assert_eq!(heartbeat_interval(&app, &token, service_id).await, 3600);

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &uri,
        &serde_json::json!({"sync_interval_secs": null}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "clear cadence ({status}): {body}");
    assert!(heartbeat_interval(&app, &token, service_id).await.is_null());

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &uri,
        &serde_json::json!({"full_reassert_enabled": false}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "switch full re-assert ({status}): {body}");
    let stored = services::Entity::find_by_id(service_id)
        .one(&db)
        .await
        .expect("query")
        .expect("service row");
    assert!(
        !stored.full_reassert_enabled,
        "the switch is stored: {body}"
    );

    let (status, body) = crate::common::patch_json_with_token(
        &app,
        &uri,
        &serde_json::json!({"sync_interval_secs": 3600}),
        &admin,
    )
    .await;
    assert_eq!(status, 405, "the row route has no PATCH ({status}): {body}");
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
    assert_eq!(
        status, 400,
        "invalid command name rejected (it is trigger_full_sync)"
    );

    let (status, resync) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/sync/services/{service_id}/commands"),
        &serde_json::json!({"command": "resync_streams", "payload": {"source_keys": ["FP3:DOC_avg_ppb:reps"]}}),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "resync_streams with source_keys ({status}): {resync}"
    );
    assert_eq!(resync["payload"]["source_keys"][0], "FP3:DOC_avg_ppb:reps");

    let (status, _) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/services/{service_id}/commands"),
        &serde_json::json!({"command": "resync_streams", "payload": {"source_keys": []}}),
        &admin,
    )
    .await;
    assert_eq!(status, 400, "resync_streams with no source_keys is refused");

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
        pending
            .iter()
            .any(|c| c["id"] == serde_json::json!(command_id) && c["command"] == "trigger_sync"),
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

    let found = SyncEvent::get_one(&db, event_id.parse().expect("event uuid"))
        .await
        .expect("the event reads back");
    assert_eq!(found.readings_synced, 42);
    assert_eq!(found.status, "completed");
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
            &format!(
                "SELECT count(*) AS c FROM sync_service_tokens WHERE service_id = '{service_id}'"
            ),
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
            &format!(
                "SELECT count(*) AS c FROM sync_services WHERE id = '{service_id}' AND paused"
            ),
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
        db.execute_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "INSERT INTO sync_events (service_id, event_type, status, started_at) \
                 VALUES ('{service_id}', 'scheduled', 'running', NOW() - INTERVAL '{age}')"
            ),
        ))
        .await
        .unwrap_or_else(|e| panic!("insert {label} event: {e}"));
    }

    let closed = river_db::routes::private::sync::flows::sweep_stale_sync_events(&db, 3600)
        .await
        .expect("sweep");
    assert_eq!(closed, 1, "only the stale event is closed");

    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sync_events WHERE status = 'failed'"
        )
        .await,
        1,
        "stale event failed"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sync_events WHERE status = 'running'"
        )
        .await,
        1,
        "fresh event untouched"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sync_events \
             WHERE status = 'failed' AND completed_at IS NOT NULL \
               AND errors @> '[\"Closed by sweeper: service stopped reporting\"]'::jsonb"
        )
        .await,
        1,
        "the closed row says who closed it and when"
    );
}

/// The session token cache is a process-global `LazyLock`, so a rotation that appears to work
/// per-request can still be minting a token row per heartbeat. Both halves are asserted: the
/// token the caller sees, and the row count behind it.
#[tokio::test]
#[serial]
async fn heartbeat_reuses_the_cached_session_token() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_sync_credentials(&db, "svc_cache", "cache-secret", "test").await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::post_json(
        &app,
        "/api/sync/enroll",
        &serde_json::json!({
            "client_id": "svc_cache",
            "client_secret": "cache-secret",
            "instance_id": "inst-cache",
        }),
    )
    .await;
    assert_eq!(status, 200, "enroll ({status}): {body}");
    let enrolled: serde_json::Value = serde_json::from_str(&body).unwrap();
    let service_id = enrolled["service_id"].as_str().unwrap().to_string();
    let enrolled_token = enrolled["session_token"].as_str().unwrap().to_string();

    for beat in 0..2 {
        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/sync/heartbeat",
            &serde_json::json!({ "service_id": service_id, "status": "idle" }),
            &enrolled_token,
        )
        .await;
        assert_eq!(status, 200, "heartbeat {beat} ({status}): {body}");
        let beat_body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            beat_body["session_token"].as_str().unwrap(),
            enrolled_token,
            "heartbeat {beat} returned a different token"
        );
    }

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) AS c FROM sync_service_tokens WHERE service_id = '{service_id}'"
            )
        )
        .await,
        1,
        "heartbeats must not mint a token row each"
    );
}

#[tokio::test]
#[serial]
async fn re_enrolling_replaces_the_cached_session_token() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_sync_credentials(&db, "svc_recache", "recache-secret", "test").await;
    let app = crate::common::build_test_app(db.clone());

    let enroll = || async {
        let (status, body) = crate::common::post_json(
            &app,
            "/api/sync/enroll",
            &serde_json::json!({
                "client_id": "svc_recache",
                "client_secret": "recache-secret",
                "instance_id": "inst-recache",
            }),
        )
        .await;
        assert_eq!(status, 200, "enroll ({status}): {body}");
        serde_json::from_str::<serde_json::Value>(&body).unwrap()
    };

    let first = enroll().await;
    let second = enroll().await;
    assert_ne!(
        first["session_token"], second["session_token"],
        "re-enrolling must issue a fresh token"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({ "service_id": second["service_id"], "status": "idle" }),
        second["session_token"].as_str().unwrap(),
    )
    .await;
    assert_eq!(status, 200, "heartbeat on the new token ({status}): {body}");
    let beat: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(beat["session_token"], second["session_token"]);
}

/// Poll until `sql` reports `expected`, or fail. The two cleanups below run in detached tasks,
/// so a fixed sleep would be either flaky or slow.
async fn poll_count(db: &DatabaseConnection, sql: &str, expected: i64, what: &str) {
    for _ in 0..100 {
        if count(db, sql).await == expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!(
        "{what}: still {} after 10s, expected {expected}",
        count(db, sql).await
    );
}

#[tokio::test]
#[serial]
async fn expired_session_tokens_are_swept_per_service_on_enroll() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_sync_credentials(&db, "svc_sweep", "sweep-secret", "test").await;
    let app = crate::common::build_test_app(db.clone());

    let (_raw, bystander) = crate::common::seed_sync_session_token(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sync_service_tokens (id, service_id, token_hash, expires_at, created_at) \
             VALUES (gen_random_uuid(), '{bystander}', 'stale-bystander', now() - interval '1 hour', now())"
        ),
    )
    .await;

    let (status, body) = crate::common::post_json(
        &app,
        "/api/sync/enroll",
        &serde_json::json!({
            "client_id": "svc_sweep",
            "client_secret": "sweep-secret",
            "instance_id": "inst-sweep",
        }),
    )
    .await;
    assert_eq!(status, 200, "enroll ({status}): {body}");
    let service_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["service_id"]
        .as_str()
        .unwrap()
        .to_string();

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sync_service_tokens (id, service_id, token_hash, expires_at, created_at) \
             VALUES (gen_random_uuid(), '{service_id}', 'stale-own', now() - interval '1 hour', now())"
        ),
    )
    .await;

    let (status, _body) = crate::common::post_json(
        &app,
        "/api/sync/enroll",
        &serde_json::json!({
            "client_id": "svc_sweep",
            "client_secret": "sweep-secret",
            "instance_id": "inst-sweep",
        }),
    )
    .await;
    assert_eq!(status, 200);

    poll_count(
        &db,
        &format!(
            "SELECT COUNT(*) AS c FROM sync_service_tokens \
             WHERE service_id = '{service_id}' AND token_hash = 'stale-own'"
        ),
        0,
        "the service's own expired token",
    )
    .await;
    assert!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) AS c FROM sync_service_tokens \
                 WHERE service_id = '{service_id}' AND expires_at > now()"
            )
        )
        .await
            > 0,
        "the sweep takes only expired rows, the live session survives"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) AS c FROM sync_service_tokens \
                 WHERE service_id = '{bystander}' AND token_hash = 'stale-bystander'"
            )
        )
        .await,
        1,
        "the sweep is scoped to the enrolling service"
    );
}

#[tokio::test]
#[serial]
async fn pending_commands_past_expiry_are_hidden_then_marked_expired() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (session_token, service_id) = crate::common::seed_sync_session_token(&db).await;

    let expired = uuid::Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sync_commands (id, service_id, command, status, created_at, expires_at) \
             VALUES ('{expired}', '{service_id}', 'trigger_sync', 'pending', now(), now() - interval '1 minute')"
        ),
    )
    .await;

    let app = crate::common::build_test_app(db.clone());
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({ "service_id": service_id, "status": "idle" }),
        &session_token,
    )
    .await;
    assert_eq!(status, 200, "heartbeat ({status}): {body}");
    let beat: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        beat["pending_commands"].as_array().unwrap().is_empty(),
        "an expired command must not be delivered: {beat}"
    );

    poll_count(
        &db,
        &format!(
            "SELECT COUNT(*) AS c FROM sync_commands WHERE id = '{expired}' AND status = 'expired'"
        ),
        1,
        "the expired command's status",
    )
    .await;
}

/// The three control plane durations are what services in the field were enrolled under, so they
/// are asserted on the rows themselves rather than on a constant: a session lasts 15 minutes and
/// an issued command stays deliverable for 5.
#[tokio::test]
#[serial]
async fn session_and_command_lifetimes_match_the_configured_defaults() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_sync_credentials(&db, "svc_ttl", "ttl-secret", "test").await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::post_json(
        &app,
        "/api/sync/enroll",
        &serde_json::json!({
            "client_id": "svc_ttl",
            "client_secret": "ttl-secret",
            "instance_id": "inst-ttl",
        }),
    )
    .await;
    assert_eq!(status, 200, "enroll ({status}): {body}");
    let service_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["service_id"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) AS c FROM sync_service_tokens WHERE service_id = '{service_id}' \
                 AND expires_at BETWEEN now() + interval '880 seconds' AND now() + interval '920 seconds'"
            )
        )
        .await,
        1,
        "session token TTL is 900s"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/services/{service_id}/commands"),
        &serde_json::json!({ "command": "trigger_sync" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "issue command ({status}): {body}");

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) AS c FROM sync_commands WHERE service_id = '{service_id}' \
                 AND expires_at BETWEEN now() + interval '280 seconds' AND now() + interval '320 seconds'"
            )
        )
        .await,
        1,
        "command expiry is 300s"
    );
}

/// Scenario: a service's last cycle reported errors (a refused window, a scope rejection). The
/// System page's service row has a slot for exactly that.
///
/// Expected behaviour: the slot is filled from the cycle ledger. `sync_services.last_error` has
/// never had a writer and the heartbeat carries no field to report one through, so reading the
/// column would leave the line permanently blank while the reason sat in `sync_events`.
#[tokio::test]
#[serial]
async fn a_service_reports_the_error_of_its_most_recent_failing_cycle() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (_token, service_id) = crate::common::seed_sync_session_token(&db).await;

    let listed = || async {
        SyncService::get_all(
            &db,
            &sea_orm::Condition::all(),
            services::Column::CreatedAt,
            sea_orm::Order::Desc,
            0,
            50,
        )
        .await
        .expect("the service list")
    };
    assert!(
        listed().await[0].last_error.is_none(),
        "a service with no failing cycle reports none"
    );

    for (started, errors) in [
        (
            "2025-06-01T08:00:00Z",
            r#"["stn:old:reps: ingest failed (HTTP 400 ...), 3 deferred"]"#,
        ),
        (
            "2025-06-01T09:00:00Z",
            r#"["stn:x:reps: ingest failed (HTTP 400 from /api/ingest: a completeness window is only accepted on a stream declared spot), 12 readings deferred to next cycle"]"#,
        ),
    ] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO sync_events \
                     (id, service_id, event_type, status, readings_synced, readings_skipped, \
                      status_events_synced, errors, started_at) \
                 VALUES (gen_random_uuid(), '{service_id}', 'sync', 'partial', 0, 0, 0, \
                         '{errors}'::jsonb, '{started}')"
            ),
        )
        .await;
    }

    let rows = listed().await;
    let reported = rows[0].last_error.as_deref().expect("an error line");
    assert!(
        reported.contains("only accepted on a stream declared spot"),
        "the newest cycle's first error, with the server's reason: {reported}"
    );

    let one = SyncService::get_one(&db, service_id)
        .await
        .expect("the service detail");
    assert_eq!(
        one.last_error.as_deref(),
        Some(reported),
        "the detail agrees with the listing"
    );
}
