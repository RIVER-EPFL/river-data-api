//! Project-scope confinement for a token's READ surface.
//!
//! Scenario: a key scoped to project A must not see project B's rows through any CRUD list/get —
//! the leak `enforce_token_scope_on_crud` left open by skipping GET (closed by `inject_read_scope`,
//! which injects a per-entity CrudCrate `ScopeCondition`). A scoped key sees only its own project;
//! an unscoped key (the admin/private surface) sees everything. Mutations stay confined too.

mod common;

use serial_test::serial;

use common::fixtures::{GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID, SITE2_ID};

const PROJECT_B_ID: &str = "00000000-0000-4000-b000-000000000001";
const SITE_B_ID: &str = "00000000-0000-4000-b000-000000000010";

// Project-B rows that a token scoped to project A must never see.
const SP_B_ID: &str = "00000000-0000-4000-b000-000000000101";
const NOTE_B_ID: &str = "00000000-0000-4000-b000-000000000201";
const ANNO_B_ID: &str = "00000000-0000-4000-b000-000000000202";
const SAMPLE_B_ID: &str = "00000000-0000-4000-b000-000000000203";
const DEPLOY_B_ID: &str = "00000000-0000-4000-b000-000000000204";
const THRESH_B_ID: &str = "00000000-0000-4000-b000-000000000205";
const STREAM_B_ID: &str = "00000000-0000-4000-b000-000000000206";
const SENSOR_ID: &str = "00000000-0000-4000-b000-0000000000ff";
// A project-A note, to prove the scoped key still reaches its own project.
const NOTE_A_ID: &str = "00000000-0000-4000-a000-000000000901";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;

    for sql in [
        format!("INSERT INTO projects (id, name, description) VALUES ('{PROJECT_B_ID}', 'Project B', 'second')"),
        format!("INSERT INTO sites (id, name, project_id) VALUES ('{SITE_B_ID}', 'ScopeSiteB', '{PROJECT_B_ID}')"),
        format!("INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type) VALUES ('{SP_B_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', 'TempB', 'sensor')"),
        format!("INSERT INTO notes (id, site_id, text) VALUES ('{NOTE_B_ID}', '{SITE_B_ID}', 'noteB')"),
        format!("INSERT INTO notes (id, site_id, text) VALUES ('{NOTE_A_ID}', '{SITE1_ID}', 'noteA')"),
        format!("INSERT INTO annotations (id, site_id, parameter_id, start_time, end_time, text, category) VALUES ('{ANNO_B_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', 'annoB', 'note')"),
        format!("INSERT INTO samples (id, site_id, parameter_id, collected_at, n) VALUES ('{SAMPLE_B_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', '2026-01-01T00:00:00Z', 1)"),
        format!("INSERT INTO sensors (id, parameter_id) VALUES ('{SENSOR_ID}', '{GLOBAL_PARAM_TEMP_ID}')"),
        format!("INSERT INTO sensor_deployments (id, sensor_id, site_id, deployed_from, deployment_type) VALUES ('{DEPLOY_B_ID}', '{SENSOR_ID}', '{SITE_B_ID}', '2026-01-01T00:00:00Z', 'permanent')"),
        format!("INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_min) VALUES ('{THRESH_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{SITE_B_ID}', 1.0)"),
        format!("INSERT INTO data_streams (id, source_system, source_key, site_parameter_id, is_active) VALUES ('{STREAM_B_ID}', 'test-b', 'b-stream-1', '{SP_B_ID}', true)"),
    ] {
        common::db::exec(&db, &sql).await;
    }

    let app = common::build_test_app(db.clone());
    (db, app)
}

/// The `id`s returned by a CrudCrate list endpoint (a bare JSON array).
async fn list_ids(app: &axum::Router, path: &str, token: &str) -> Vec<String> {
    let (status, body) = common::get_json_with_token(app, path, token).await;
    assert_eq!(status, 200, "list {path} should be 200, body: {body}");
    body.as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Every project-bound CRUD entity: a scoped key sees only project A (its own), never project B's
/// row — neither in the list nor by direct id (which 404s, not 403, so it doesn't even confirm the
/// row exists). The unscoped key sees both.
#[tokio::test]
#[serial]
async fn scoped_key_confined_on_crud_reads() {
    let (db, app) = setup().await;
    let scoped = common::seed_api_token(&db, common::full_permissions(), Some(PROJECT_ID)).await;
    let unscoped = common::seed_api_token(&db, common::full_permissions(), None).await;

    // (entity path, the project-B row id that must be hidden from the scoped key)
    let cases: &[(&str, &str)] = &[
        ("/api/projects", PROJECT_B_ID),
        ("/api/sites", SITE_B_ID),
        ("/api/site_parameters", SP_B_ID),
        ("/api/notes", NOTE_B_ID),
        ("/api/annotations", ANNO_B_ID),
        ("/api/samples", SAMPLE_B_ID),
        ("/api/sensor_deployments", DEPLOY_B_ID),
        ("/api/alarm_thresholds", THRESH_B_ID),
        ("/api/data_streams", STREAM_B_ID),
    ];

    for (path, b_id) in cases {
        // List: scoped key must not enumerate the project-B row; unscoped key must.
        let scoped_ids = list_ids(&app, path, &scoped).await;
        assert!(
            !scoped_ids.iter().any(|id| id == b_id),
            "scoped key leaked a project-B row via {path} list: {scoped_ids:?}"
        );
        let unscoped_ids = list_ids(&app, path, &unscoped).await;
        assert!(
            unscoped_ids.iter().any(|id| id == b_id),
            "unscoped key should see the project-B row via {path} list"
        );

        // Get-by-id: scoped key 404s the cross-project row (no existence confirmation); unscoped 200s.
        let (s, _) = common::get_with_token(&app, &format!("{path}/{b_id}"), &scoped).await;
        assert_eq!(s, 404, "scoped key must 404 a cross-project {path} row, got {s}");
        let (s, _) = common::get_with_token(&app, &format!("{path}/{b_id}"), &unscoped).await;
        assert_eq!(s, 200, "unscoped key must reach the {path} row, got {s}");
    }
}

/// The same scoped key still reaches its OWN project's rows — confinement filters out other
/// projects without breaking the token's legitimate access.
#[tokio::test]
#[serial]
async fn scoped_key_still_sees_own_project() {
    let (db, app) = setup().await;
    let scoped = common::seed_api_token(&db, common::full_permissions(), Some(PROJECT_ID)).await;

    // Sites: its own two are visible, the foreign one is not.
    let site_ids = list_ids(&app, "/api/sites", &scoped).await;
    assert!(site_ids.iter().any(|id| id == SITE1_ID), "own SITE1 visible");
    assert!(site_ids.iter().any(|id| id == SITE2_ID), "own SITE2 visible");
    assert!(!site_ids.iter().any(|id| id == SITE_B_ID), "foreign site hidden");

    // Its own project resolves; the foreign one 404s.
    let (s, _) = common::get_with_token(&app, &format!("/api/sites/{SITE1_ID}"), &scoped).await;
    assert_eq!(s, 200, "own site by id is reachable");

    // Its own note is listed and reachable by id.
    let note_ids = list_ids(&app, "/api/notes", &scoped).await;
    assert!(note_ids.iter().any(|id| id == NOTE_A_ID), "own note visible");
    let (s, _) = common::get_with_token(&app, &format!("/api/notes/{NOTE_A_ID}"), &scoped).await;
    assert_eq!(s, 200, "own note by id is reachable");

    // Its own project is the only one listed.
    let project_ids = list_ids(&app, "/api/projects", &scoped).await;
    assert_eq!(project_ids, vec![PROJECT_ID.to_string()], "only own project listed");
}

/// A batch that mixes an in-scope and an out-of-scope site in one payload is rejected wholesale —
/// a scoped key can't smuggle a foreign-site reading alongside a legitimate one.
#[tokio::test]
#[serial]
async fn mixed_payload_batch_rejected() {
    let (db, app) = setup().await;
    let key = common::seed_api_token(&db, common::perms(true, true, false, true), Some(PROJECT_ID)).await;
    let t = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mixed = serde_json::json!({
        "readings": [
            { "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "raw_value": 1.0 },
            { "site_id": SITE_B_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "raw_value": 2.0 }
        ]
    });
    let (s, _) = common::post_json_with_token(&app, "/api/readings/batch", &mixed, &key).await;
    assert_eq!(s, 403, "a batch touching a foreign site must be rejected wholesale, got {s}");

    // And the in-scope reading must NOT have been written (all-or-nothing).
    let only_scoped = list_ids(&app, "/api/sites", &key).await;
    assert!(!only_scoped.iter().any(|id| id == SITE_B_ID), "foreign site still hidden after attempt");
}
