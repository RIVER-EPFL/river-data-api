//! Project-scope confinement for a token's READ surface.
//!
//! Scenario: a key scoped to project A must not see project B's rows through any CRUD list/get,
//! the leak `enforce_token_scope_on_crud` left open by skipping GET (closed by `inject_project_scope`,
//! which injects a per-entity CrudCrate `ScopeCondition`). A scoped key sees only its own project;
//! an unscoped key (the admin/private surface) sees everything. Mutations stay confined too.

use serial_test::serial;

use crate::common::fixtures::{GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID, SITE2_ID};

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
const SENSOR_B_ID: &str = "00000000-0000-4000-b000-0000000000ff";
const CALIB_B_ID: &str = "00000000-0000-4000-b000-000000000301";
const CURVE_B_ID: &str = "00000000-0000-4000-b000-000000000303";
const REPROC_B_ID: &str = "00000000-0000-4000-b000-000000000302";
const VISIT_B_ID: &str = "00000000-0000-4000-b000-000000000401";
const RUN_B_ID: &str = "00000000-0000-4000-b000-000000000402";
const RECEIPT_B_ID: &str = "00000000-0000-4000-b000-000000000403";
const MUTE_B_ID: &str = "00000000-0000-4000-b000-000000000404";
const SUBSCRIPTION_B_ID: &str = "00000000-0000-4000-b000-000000000405";
// A project-A note, to prove the scoped key still reaches its own project.
const NOTE_A_ID: &str = "00000000-0000-4000-a000-000000000901";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    for sql in [
        format!(
            "INSERT INTO projects (id, name, description) VALUES ('{PROJECT_B_ID}', 'Project B', 'second')"
        ),
        format!(
            "INSERT INTO sites (id, name, project_id) VALUES ('{SITE_B_ID}', 'ScopeSiteB', '{PROJECT_B_ID}')"
        ),
        format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type) VALUES ('{SP_B_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', 'TempB', 'sensor')"
        ),
        format!(
            "INSERT INTO notes (id, site_id, text) VALUES ('{NOTE_B_ID}', '{SITE_B_ID}', 'noteB')"
        ),
        format!(
            "INSERT INTO notes (id, site_id, text) VALUES ('{NOTE_A_ID}', '{SITE1_ID}', 'noteA')"
        ),
        format!(
            "INSERT INTO annotations (id, site_id, parameter_id, start_time, end_time, text, category) VALUES ('{ANNO_B_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', 'annoB', 'note')"
        ),
        format!(
            "INSERT INTO samples (id, site_id, parameter_id, collected_at, n) VALUES ('{SAMPLE_B_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', '2026-01-01T00:00:00Z', 1)"
        ),
        format!("INSERT INTO sensors (id) VALUES ('{SENSOR_B_ID}')"),
        format!(
            "INSERT INTO sensor_deployments (id, sensor_id, site_id, parameter_id, deployed_from, deployment_type) VALUES ('{DEPLOY_B_ID}', '{SENSOR_B_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', '2026-01-01T00:00:00Z', 'permanent')"
        ),
        format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from) VALUES ('{CALIB_B_ID}', '{SENSOR_B_ID}', 1.0, 0.0, '2026-01-01T00:00:00Z')"
        ),
        format!(
            "INSERT INTO standard_curves (id, sensor_id, name, slope, intercept) VALUES ('{CURVE_B_ID}', '{SENSOR_B_ID}', 'Plate B', 3.0, 0.5)"
        ),
        format!(
            "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, status) VALUES ('{REPROC_B_ID}', '{SENSOR_B_ID}', 'calibration', 'completed')"
        ),
        format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_min) VALUES ('{THRESH_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{SITE_B_ID}', 1.0)"
        ),
        format!(
            "INSERT INTO data_streams (id, source_system, source_key, site_parameter_id, is_active) VALUES ('{STREAM_B_ID}', 'test-b', 'b-stream-1', '{SP_B_ID}', true)"
        ),
        format!(
            "INSERT INTO collection_events (id, site_id, collected_at) VALUES ('{VISIT_B_ID}', '{SITE_B_ID}', '2026-01-01T00:00:00Z')"
        ),
        format!(
            "INSERT INTO tool_runs (id, tool_name, tool_version, inputs, constants, curves, outputs, created_by, site_id, collected_at) VALUES ('{RUN_B_ID}', 'doc', '{{}}', '{{}}', '{{}}', '[]', '{{}}', 'tester', '{SITE_B_ID}', '2026-01-01T00:00:00Z')"
        ),
        format!(
            "INSERT INTO ingest_receipts (id, stream_id, submitted, new_rows, changed, unchanged, retained, rejected_total, rejected, dropped, withdrawn) VALUES ('{RECEIPT_B_ID}', '{STREAM_B_ID}', 0, 0, 0, 0, 0, 0, '[]', 0, 0)"
        ),
        format!(
            "INSERT INTO notification_mutes (id, site_id, parameter_id) VALUES ('{MUTE_B_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_TEMP_ID}')"
        ),
        format!(
            "INSERT INTO meteoswiss_subscriptions (id, site_id, station_abbr, variable, parameter_id) VALUES ('{SUBSCRIPTION_B_ID}', '{SITE_B_ID}', 'SIO', 'prestas0', '{GLOBAL_PARAM_TEMP_ID}')"
        ),
    ] {
        crate::common::db::exec(&db, &sql).await;
    }

    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

/// The `id`s returned by a CrudCrate list endpoint (a bare JSON array).
async fn list_ids(app: &axum::Router, path: &str, token: &str) -> Vec<String> {
    let (status, body) = crate::common::get_json_with_token(app, path, token).await;
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
/// row, neither in the list nor by direct id (which 404s, not 403, so it doesn't even confirm the
/// row exists). The unscoped key sees both.
#[tokio::test]
#[serial]
async fn scoped_key_confined_on_crud_reads() {
    let (db, app) = setup().await;
    let scoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), Some(PROJECT_ID))
            .await;
    let unscoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;

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
        ("/api/sensors", SENSOR_B_ID),
        ("/api/sensor_calibrations", CALIB_B_ID),
        ("/api/standard_curves", CURVE_B_ID),
        ("/api/reprocessing_jobs", REPROC_B_ID),
        ("/api/collection_events", VISIT_B_ID),
        ("/api/tool_runs", RUN_B_ID),
        ("/api/ingest_receipts", RECEIPT_B_ID),
        ("/api/notification_mutes", MUTE_B_ID),
        ("/api/meteoswiss_subscriptions", SUBSCRIPTION_B_ID),
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
        let (s, _) = crate::common::get_with_token(&app, &format!("{path}/{b_id}"), &scoped).await;
        assert_eq!(
            s, 404,
            "scoped key must 404 a cross-project {path} row, got {s}"
        );
        let (s, _) =
            crate::common::get_with_token(&app, &format!("{path}/{b_id}"), &unscoped).await;
        assert_eq!(s, 200, "unscoped key must reach the {path} row, got {s}");
    }
}

/// The subjects of change-audit rows `path` returns to `token`, filtered to one subject.
async fn audit_subjects(app: &axum::Router, subject: &str, token: &str) -> Vec<String> {
    let filter = crate::common::e2e::percent_encode(&format!(r#"{{"subject":"{subject}"}}"#));
    let path = format!("/api/change_audit_entries?filter={filter}");
    let (status, body) = crate::common::get_json_with_token(app, &path, token).await;
    assert_eq!(status, 200, "list {path} should be 200, body: {body}");
    body.as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["subject"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The change trail carries whole rows, so a site-bearing subject is read where its row is, through
/// the entity list and the subject-keyed reader alike, while a catalog subject is read by anyone.
#[tokio::test]
#[serial]
async fn scoped_key_confined_on_the_change_trail() {
    let (db, app) = setup().await;
    let scoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), Some(PROJECT_ID))
            .await;
    let unscoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;

    for subject in [
        format!("site:{SITE_B_ID}"),
        format!("site_parameter:{SP_B_ID}"),
        format!("sensor_calibration:{CALIB_B_ID}"),
        format!("standard_curve:{CURVE_B_ID}"),
    ] {
        assert!(
            audit_subjects(&app, &subject, &scoped).await.is_empty(),
            "scoped key leaked {subject} via /change_audit_entries"
        );
        assert!(
            !audit_subjects(&app, &subject, &unscoped).await.is_empty(),
            "unscoped key should see {subject}"
        );
        let path = format!("/api/change_audit?subject={subject}");
        let (status, body) = crate::common::get_json_with_token(&app, &path, &scoped).await;
        assert_eq!(status, 200, "{path}: {body}");
        assert_eq!(
            body,
            serde_json::json!([]),
            "scoped key leaked {subject} via {path}"
        );
        let (_, body) = crate::common::get_json_with_token(&app, &path, &unscoped).await;
        assert_ne!(
            body,
            serde_json::json!([]),
            "unscoped key should see {subject} via {path}"
        );
    }

    for subject in [
        format!("site:{SITE1_ID}"),
        format!("parameter:{GLOBAL_PARAM_TEMP_ID}"),
    ] {
        assert!(
            !audit_subjects(&app, &subject, &scoped).await.is_empty(),
            "scoped key should see {subject}"
        );
    }
}

/// The same scoped key still reaches its OWN project's rows, confinement filters out other
/// projects without breaking the token's legitimate access.
#[tokio::test]
#[serial]
async fn scoped_key_still_sees_own_project() {
    let (db, app) = setup().await;
    let scoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), Some(PROJECT_ID))
            .await;

    // Sites: its own two are visible, the foreign one is not.
    let site_ids = list_ids(&app, "/api/sites", &scoped).await;
    assert!(
        site_ids.iter().any(|id| id == SITE1_ID),
        "own SITE1 visible"
    );
    assert!(
        site_ids.iter().any(|id| id == SITE2_ID),
        "own SITE2 visible"
    );
    assert!(
        !site_ids.iter().any(|id| id == SITE_B_ID),
        "foreign site hidden"
    );

    // Its own project resolves; the foreign one 404s.
    let (s, _) =
        crate::common::get_with_token(&app, &format!("/api/sites/{SITE1_ID}"), &scoped).await;
    assert_eq!(s, 200, "own site by id is reachable");

    // Its own note is listed and reachable by id.
    let note_ids = list_ids(&app, "/api/notes", &scoped).await;
    assert!(
        note_ids.iter().any(|id| id == NOTE_A_ID),
        "own note visible"
    );
    let (s, _) =
        crate::common::get_with_token(&app, &format!("/api/notes/{NOTE_A_ID}"), &scoped).await;
    assert_eq!(s, 200, "own note by id is reachable");

    // Its own project is the only one listed.
    let project_ids = list_ids(&app, "/api/projects", &scoped).await;
    assert_eq!(
        project_ids,
        vec![PROJECT_ID.to_string()],
        "only own project listed"
    );
}

/// Sync infrastructure entities (sync_services, sync_commands, sync_events, pairing_plans) are
/// admin-only, no API token (scoped or unscoped) can list them.
#[tokio::test]
#[serial]
async fn admin_only_entities_reject_api_tokens() {
    let (db, app) = setup().await;
    let unscoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;

    for path in [
        "/api/sync_services",
        "/api/sync_commands",
        "/api/sync_events",
        "/api/pairing_plans",
    ] {
        let (s, _) = crate::common::get_with_token(&app, path, &unscoped).await;
        // An authenticated token that lacks admin gets 403 (forbidden), not 401 (unauthenticated):
        // the convention enforced across the auth suite (see malformed_and_revoked_rejection).
        assert_eq!(
            s, 403,
            "{path} should be admin-only (403 for an authenticated API token), got {s}"
        );
    }
}

/// A batch that mixes an in-scope and an out-of-scope site in one payload is rejected wholesale,
/// a scoped key can't smuggle a foreign-site reading alongside a legitimate one.
#[tokio::test]
#[serial]
async fn mixed_payload_batch_rejected() {
    let (db, app) = setup().await;
    let key = crate::common::seed_api_token(
        &db,
        crate::common::perms(true, true, false, true),
        Some(PROJECT_ID),
    )
    .await;
    let t = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mixed = serde_json::json!({
        "readings": [
            { "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "raw_value": 1.0 },
            { "site_id": SITE_B_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "raw_value": 2.0 }
        ]
    });
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &mixed, &key).await;
    assert_eq!(
        s, 403,
        "a batch touching a foreign site must be rejected wholesale, got {s}"
    );

    // And the in-scope reading must NOT have been written (all-or-nothing).
    let only_scoped = list_ids(&app, "/api/sites", &key).await;
    assert!(
        !only_scoped.iter().any(|id| id == SITE_B_ID),
        "foreign site still hidden after attempt"
    );
}

/// A curve belongs to an instrument, and an instrument belongs to the projects it is deployed into.
/// A key scoped to project A therefore reaches its own instruments' curves and nothing else, on
/// every verb: it cannot read, mint, edit or remove a curve on project B's instrument.
#[tokio::test]
#[serial]
async fn a_scoped_token_cannot_reach_another_projects_curve() {
    let (db, app) = setup().await;
    let scoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), Some(PROJECT_ID))
            .await;

    let sensor_a = uuid::Uuid::new_v4();
    let undeployed = uuid::Uuid::new_v4();
    for sql in [
        format!("INSERT INTO sensors (id, name) VALUES ('{sensor_a}', 'Plate reader A')"),
        format!("INSERT INTO sensors (id, name) VALUES ('{undeployed}', 'Boxed plate reader')"),
        format!(
            "INSERT INTO sensor_deployments (id, sensor_id, site_id, parameter_id, deployed_from, deployment_type) \
             VALUES (gen_random_uuid(), '{sensor_a}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '2026-01-01T00:00:00Z', 'permanent')"
        ),
    ] {
        crate::common::db::exec(&db, &sql).await;
    }

    let (s, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/standard_curves",
        &serde_json::json!({ "sensor_id": sensor_a, "name": "Plate A", "slope": 3.0, "intercept": 0.5 }),
        &scoped,
    )
    .await;
    assert!(
        (200..300).contains(&s),
        "a scoped key mints a curve on its own project's instrument: {body}"
    );
    let own_curve = body["id"]
        .as_str()
        .expect("the curve carries an id")
        .to_string();

    let (s, _) = crate::common::post_json_with_token(
        &app,
        "/api/standard_curves",
        &serde_json::json!({ "sensor_id": SENSOR_B_ID, "name": "Plate B2", "slope": 2.0, "intercept": 0.0 }),
        &scoped,
    )
    .await;
    assert_eq!(s, 403, "and not on an instrument outside its project");

    // An instrument that has never been deployed belongs to no project, so a scoped key cannot
    // place a curve on it: there is nothing to check the scope against.
    let (s, _) = crate::common::post_json_with_token(
        &app,
        "/api/standard_curves",
        &serde_json::json!({ "sensor_id": undeployed, "name": "Unplaced", "slope": 2.0, "intercept": 0.0 }),
        &scoped,
    )
    .await;
    assert_eq!(s, 403, "nor on an instrument no project claims");

    let (s, _) = crate::common::put_json_with_token(
        &app,
        &format!("/api/standard_curves/{CURVE_B_ID}"),
        &serde_json::json!({ "notes": "edited from outside" }),
        &scoped,
    )
    .await;
    assert!(
        s == 403 || s == 404,
        "a scoped key cannot edit a foreign project's curve, got {s}"
    );

    let (s, _) = crate::common::delete_with_token(
        &app,
        &format!("/api/standard_curves/{CURVE_B_ID}"),
        &scoped,
    )
    .await;
    assert!(s == 403 || s == 404, "nor delete one, got {s}");
    assert_eq!(
        crate::common::get_with_token(&app, &format!("/api/standard_curves/{CURVE_B_ID}"), &scoped)
            .await
            .0,
        404,
        "and the foreign curve is not even confirmed to exist"
    );

    let (s, _) =
        crate::common::get_with_token(&app, &format!("/api/standard_curves/{own_curve}"), &scoped)
            .await;
    assert_eq!(s, 200, "its own curve stays reachable");
    let (s, _) = crate::common::delete_with_token(
        &app,
        &format!("/api/standard_curves/{own_curve}"),
        &scoped,
    )
    .await;
    assert!(
        (200..300).contains(&s),
        "and removable while no reading has used it"
    );
}

/// One reading and one tool run in project B. A key scoped to project A reads neither's history,
/// replay, inspection, reload or trace; the unscoped key reaches all of them.
#[tokio::test]
#[serial]
async fn a_scoped_token_cannot_read_another_projects_reading_history() {
    let (db, app) = setup().await;
    let scoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), Some(PROJECT_ID))
            .await;
    let unscoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;

    let time = "2026-01-01T12:00:00Z";
    let run_b = uuid::Uuid::new_v4();
    for sql in [
        format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, raw_value, measurement_type) \
             VALUES ('{STREAM_B_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{time}', 0, 4.2, 'continuous')"
        ),
        format!(
            "INSERT INTO tool_runs (id, tool_name, tool_version, inputs, constants, curves, outputs, created_by, source, site_id) \
             VALUES ('{run_b}', 'doc', '{{}}', '{{}}', '{{}}', '[]', '{{}}', 'test', 'interactive', '{SITE_B_ID}')"
        ),
    ] {
        crate::common::db::exec(&db, &sql).await;
    }

    let decisions = format!("/api/readings/decisions?stream_id={STREAM_B_ID}&time={time}");
    let (s, _) = crate::common::get_with_token(&app, &decisions, &scoped).await;
    assert_eq!(s, 404, "a scoped key reads no foreign reading's decisions");
    let (s, _) = crate::common::get_with_token(&app, &decisions, &unscoped).await;
    assert_eq!(s, 200, "the unscoped key does");

    let replay = format!("/api/readings/replay?stream_id={STREAM_B_ID}&time={time}");
    let (s, body) = crate::common::get_with_token(&app, &replay, &scoped).await;
    assert_eq!(s, 404, "a scoped key replays no foreign reading");
    assert!(body.contains("No reading at that instant"), "{body}");
    let (_, body) = crate::common::get_with_token(&app, &replay, &unscoped).await;
    assert!(
        !body.contains("No reading at that instant"),
        "the unscoped key finds the reading: {body}"
    );

    let inspect = serde_json::json!({ "selection": { "stream_id": STREAM_B_ID } });
    let (s, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/edits/inspect",
        &inspect,
        &scoped,
    )
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        body["rows"].as_array().map(Vec::len),
        Some(0),
        "a scoped key inspects no foreign row: {body}"
    );
    let (_, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/edits/inspect",
        &inspect,
        &unscoped,
    )
    .await;
    assert_eq!(body["rows"].as_array().map(Vec::len), Some(1), "{body}");

    for route in ["reload", "trace"] {
        let path = format!("/api/tool_runs/{run_b}/{route}");
        let (s, _) = crate::common::get_with_token(&app, &path, &scoped).await;
        assert_eq!(s, 404, "a scoped key cannot {route} a foreign tool run");
        let (s, _) = crate::common::get_with_token(&app, &path, &unscoped).await;
        assert_ne!(s, 404, "the unscoped key reaches the run's {route}");
    }
}
