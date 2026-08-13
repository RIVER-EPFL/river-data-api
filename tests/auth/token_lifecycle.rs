//! End-to-end workflow for the secure external-push API-key feature.
//!
//! Scenario: an admin issues a project-scoped key to an external logger; the logger pushes data,
//! is confined to its project on every write path (readings/grab/status/ingest + metadata CRUD),
//! is rate-limited, and the key can be rotated/revoked with immediate effect. Token *management*
//! endpoints (create/revoke/rotate) are `require_admin` (Keycloak-only) and cannot be driven by a
//! bearer token, so the admin-issued key is seeded directly (representing the create) and the
//! revoke/rotate handlers are invoked against the same `AppState` the router uses, so the
//! cache-busting is exercised for real.

use axum::extract::{Path, State};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

use crate::common::fixtures::{
    GLOBAL_PARAM_DEPTH_ID, GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID, SITE2_ID,
};

const PROJECT_B_ID: &str = "00000000-0000-4000-b000-000000000001";
const SITE_B_ID: &str = "00000000-0000-4000-b000-000000000010";

fn write_data_perms() -> serde_json::Value {
    crate::common::perms(true, true, false, true)
}
fn write_metadata_perms() -> serde_json::Value {
    crate::common::perms(true, true, true, false)
}

async fn setup_two_projects() -> (
    sea_orm::DatabaseConnection,
    axum::Router,
    river_db::common::AppState,
) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, description) \
             VALUES ('{PROJECT_B_ID}', 'Project B', 'second project')"
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, name, project_id) \
             VALUES ('{SITE_B_ID}', 'ScopeSiteB', '{PROJECT_B_ID}')"
        ),
    )
    .await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    (db, app, state)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Resolve a token's id from its public prefix (`rvd_<prefix>_<secret>`).
async fn token_id(db: &sea_orm::DatabaseConnection, raw: &str) -> uuid::Uuid {
    let prefix = raw
        .strip_prefix("rvd_")
        .and_then(|r| r.split_once('_'))
        .map(|(p, _)| p)
        .expect("api token must be rvd_<prefix>_<secret>");
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id FROM api_tokens WHERE token_prefix = $1",
            [prefix.into()],
        ))
        .await
        .unwrap()
        .expect("token row");
    row.try_get::<uuid::Uuid>("", "id").unwrap()
}

#[tokio::test]
#[serial]
async fn external_push_key_confined_to_its_project_on_every_write_path() {
    let (db, app, _state) = setup_two_projects().await;

    // An admin issued this key to a logger, scoped to Project A, with data-write permission.
    let key = crate::common::seed_api_token(&db, write_data_perms(), Some(PROJECT_ID)).await;

    // Stored securely: argon2id hash + non-secret lookup prefix (never the raw secret).
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT token_hash, token_prefix FROM api_tokens WHERE token_prefix = $1",
            [key.strip_prefix("rvd_")
                .unwrap()
                .split_once('_')
                .unwrap()
                .0
                .into()],
        ))
        .await
        .unwrap()
        .expect("token row");
    let hash: String = row.try_get("", "token_hash").unwrap();
    let prefix: String = row.try_get("", "token_prefix").unwrap();
    assert!(
        hash.starts_with("$argon2id$"),
        "secret must be argon2id-hashed, got {hash}"
    );
    assert!(!prefix.is_empty(), "token_prefix must be set for lookup");

    let t = now_rfc3339();

    // Reachability: a pure bearer-token write succeeds with NO Keycloak present.
    let batch_in = serde_json::json!({
        "readings": [{ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "raw_value": 12.3 }]
    });
    let (s, body) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &batch_in, &key).await;
    assert_eq!(s, 200, "in-scope batch write must succeed: {body}");

    // ...but the same key cannot push into Project B.
    let batch_out = serde_json::json!({
        "readings": [{ "site_id": SITE_B_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "raw_value": 9.9 }]
    });
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &batch_out, &key).await;
    assert_eq!(s, 403, "cross-project batch write must be forbidden");

    // Grab samples: in-scope ok, out-of-scope forbidden.
    let grab_in = serde_json::json!({
        "site_id": SITE1_ID,
        "readings": [{ "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 1.5, "time": t }]
    });
    let (s, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &grab_in, &key).await;
    assert_eq!(s, 200, "in-scope grab must succeed: {body}");
    let grab_out = serde_json::json!({
        "site_id": SITE_B_ID,
        "readings": [{ "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 1.5, "time": t }]
    });
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &grab_out, &key).await;
    assert_eq!(s, 403, "cross-project grab must be forbidden");

    // Status events batch: out-of-scope forbidden.
    let status_out = serde_json::json!({
        "events": [{ "site_id": SITE_B_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "value": "low_battery" }]
    });
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/status_events/batch", &status_out, &key)
            .await;
    assert_eq!(s, 403, "cross-project status events must be forbidden");

    let _ = &db;
}

#[tokio::test]
#[serial]
async fn scoped_metadata_key_confined_and_blocked_from_global_catalog() {
    let (db, app, _state) = setup_two_projects().await;

    // A scoped write_metadata key: may modify metadata within its project only.
    let key = crate::common::seed_api_token(&db, write_metadata_perms(), Some(PROJECT_ID)).await;

    // Create a site_parameter within Project A (FK resolves to Project A): allowed.
    // (SITE2, DEPTH) is unconfigured in the seed, so this won't collide.
    let sp_in = serde_json::json!({ "site_id": SITE2_ID, "parameter_id": GLOBAL_PARAM_DEPTH_ID });
    let (s, created) =
        crate::common::post_json_parse_with_token(&app, "/api/site_parameters", &sp_in, &key).await;
    assert!(
        (200..300).contains(&s),
        "in-scope site_parameter create must succeed: {created}"
    );
    let new_sp_id = created["id"]
        .as_str()
        .expect("created site_parameter id")
        .to_string();

    // The same in Project B: forbidden (create-body FK resolves to Project B).
    let sp_out = serde_json::json!({ "site_id": SITE_B_ID, "parameter_id": GLOBAL_PARAM_DEPTH_ID });
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/site_parameters", &sp_out, &key).await;
    assert_eq!(
        s, 403,
        "cross-project site_parameter create must be forbidden"
    );

    // Resolve-by-id (delete) within the project: allowed.
    let (s, _) =
        crate::common::delete_with_token(&app, &format!("/api/site_parameters/{new_sp_id}"), &key)
            .await;
    assert!(
        (200..300).contains(&s),
        "in-scope delete-by-id must succeed, got {s}"
    );

    // Resolve-by-id against a CROSS-project resource is blocked before reaching the handler.
    let (s, _) =
        crate::common::delete_with_token(&app, &format!("/api/sites/{SITE_B_ID}"), &key).await;
    assert_eq!(
        s, 403,
        "scoped key cannot delete a cross-project site by id"
    );

    // Mutating the GLOBAL catalog (a parameter) is denied for any scoped key (fail-closed).
    let new_param = serde_json::json!({ "code": "scoped_new", "name": "Scoped New", "default_units": "x", "category": "measurement", "aliases": [] });
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/parameters", &new_param, &key).await;
    assert_eq!(s, 403, "scoped key must not create global catalog entities");
}

#[tokio::test]
#[serial]
async fn scoped_key_denied_on_operator_and_global_actions() {
    let (db, app, _state) = setup_two_projects().await;
    // Full permissions, but project-scoped: operator/global actions are still denied.
    let key =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), Some(PROJECT_ID))
            .await;

    let cases: &[(&str, serde_json::Value)] = &[
        ("/api/actions/reprocess_all", serde_json::json!({})),
        ("/api/actions/refresh_aggregates", serde_json::json!({})),
        ("/api/actions/backfill_attribution", serde_json::json!({})),
        (
            "/api/sensors/00000000-0000-4000-c000-0000000000ff/adopt",
            serde_json::json!({ "site_id": SITE1_ID }),
        ),
        ("/api/streams/register", serde_json::json!({})),
    ];
    for (path, body) in cases {
        let (s, _) = crate::common::post_json_with_token(&app, path, body, &key).await;
        assert_eq!(s, 403, "scoped key must be denied on {path}, got {s}");
    }

    // import_csv IS a reachable data-push path, but it is scope-enforced: a cross-project import
    // is rejected before any parsing.
    let csv_out = serde_json::json!({
        "csv": "DateTime,Temp\n2026-01-01T00:00:00Z,1.0",
        "site": SITE_B_ID
    });
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/readings/import_csv", &csv_out, &key).await;
    assert_eq!(
        s, 403,
        "scoped key must not import CSV into another project"
    );
}

#[tokio::test]
#[serial]
async fn per_token_rate_limit_returns_429() {
    let (db, app, _state) = setup_two_projects().await;

    // 2 requests/second ceiling on this key.
    let limited =
        crate::common::seed_api_token_with_rate_limit(&db, write_data_perms(), Some(PROJECT_ID), 2)
            .await;
    let unlimited = crate::common::seed_api_token(&db, write_data_perms(), Some(PROJECT_ID)).await;

    let t = now_rfc3339();
    let body = serde_json::json!({
        "readings": [{ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "raw_value": 1.0 }]
    });

    let mut limited_statuses = Vec::new();
    for _ in 0..8 {
        let (s, _) =
            crate::common::post_json_with_token(&app, "/api/readings/batch", &body, &limited).await;
        limited_statuses.push(s);
    }
    assert!(
        limited_statuses.iter().any(|&s| s == 429),
        "a rate-limited key must eventually 429 in a burst, got {limited_statuses:?}"
    );

    let mut unlimited_statuses = Vec::new();
    for _ in 0..8 {
        let (s, _) =
            crate::common::post_json_with_token(&app, "/api/readings/batch", &body, &unlimited)
                .await;
        unlimited_statuses.push(s);
    }
    assert!(
        unlimited_statuses.iter().all(|&s| s != 429),
        "a key with no configured limit must never 429, got {unlimited_statuses:?}"
    );
}

#[tokio::test]
#[serial]
async fn revoke_and_rotate_take_effect_immediately() {
    let (db, app, state) = setup_two_projects().await;

    // --- Revoke ---
    let revoke_key = crate::common::seed_api_token(&db, write_data_perms(), Some(PROJECT_ID)).await;
    let revoke_id = token_id(&db, &revoke_key).await;

    // Use it once so it is cached as valid.
    let (s, _) = crate::common::get_with_token(&app, "/api/sites", &revoke_key).await;
    assert_eq!(s, 200, "fresh key should authenticate");

    // Admin revokes (handler invoked against the SAME state the router shares).
    let _revoked = river_db::routes::private::api_tokens::views::revoke_token(
        State(state.clone()),
        Path(revoke_id),
    )
    .await
    .expect("revoke ok");

    // Next request fails immediately despite the still-warm cache.
    let (s, _) = crate::common::get_with_token(&app, "/api/sites", &revoke_key).await;
    assert_eq!(s, 401, "revoked key must fail on the very next request");

    // --- Rotate ---
    let rotate_key = crate::common::seed_api_token(&db, write_data_perms(), Some(PROJECT_ID)).await;
    let rotate_id = token_id(&db, &rotate_key).await;
    let (s, _) = crate::common::get_with_token(&app, "/api/sites", &rotate_key).await;
    assert_eq!(s, 200, "fresh key should authenticate before rotation");

    let rotated = river_db::routes::private::api_tokens::views::rotate_token(
        State(state.clone()),
        Path(rotate_id),
    )
    .await
    .expect("rotate ok")
    .0;
    let new_secret = rotated
        .token
        .clone()
        .expect("rotate returns the new secret once");
    assert_ne!(
        new_secret, rotate_key,
        "rotation must mint a different secret"
    );

    // Old secret is dead, new secret works, metadata (scope) preserved.
    let (s, _) = crate::common::get_with_token(&app, "/api/sites", &rotate_key).await;
    assert_eq!(s, 401, "old secret must stop working after rotation");
    let (s, _) = crate::common::get_with_token(&app, "/api/sites", &new_secret).await;
    assert_eq!(s, 200, "new secret must authenticate after rotation");
    assert_eq!(
        rotated.project_scope.map(|p| p.to_string()),
        Some(PROJECT_ID.to_string()),
        "rotation preserves project scope"
    );

    // --- Rotating a REVOKED token must not silently re-enable it ---
    let frozen_key = crate::common::seed_api_token(&db, write_data_perms(), Some(PROJECT_ID)).await;
    let frozen_id = token_id(&db, &frozen_key).await;
    let _ = river_db::routes::private::api_tokens::views::revoke_token(
        State(state.clone()),
        Path(frozen_id),
    )
    .await
    .expect("revoke ok");
    let rotated_frozen = river_db::routes::private::api_tokens::views::rotate_token(
        State(state.clone()),
        Path(frozen_id),
    )
    .await
    .expect("rotate ok")
    .0;
    assert!(
        !rotated_frozen.is_active,
        "rotating a revoked token keeps it revoked"
    );
    let new_frozen = rotated_frozen.token.clone().expect("new secret");
    let (s, _) = crate::common::get_with_token(&app, "/api/sites", &new_frozen).await;
    assert_eq!(s, 401, "a rotated-but-revoked token must not authenticate");
}
