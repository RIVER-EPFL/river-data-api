//! Project scope on the operator actions and on the alarm rows addressed by id, driven by every
//! principal the API admits: an administrator, a member with no grants, a member granted one
//! project, a member granted two, and a project-scoped API token.
//!
//! Scenario: two projects, each with a site, a resolved threshold slot, an instrument with
//! claimable history and an open alarm event; plus one instrument sitting in inventory with no
//! deployment at all.
//!
//! Expected behaviour: an enumeration carries only the caller's projects, a named target outside
//! them is refused, a target that does not exist is not found for anyone including an
//! administrator, and an action that names nothing is refused to any caller confined to a project
//! set, because an untargeted run reaches every project.

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;

use crate::common::fixtures::{GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID};
use crate::common::keycloak as kc;

const PROJECT_B_ID: &str = "00000000-0000-4000-a000-00000000ac01";
const SITE_B_ID: &str = "00000000-0000-4000-a000-00000000ac02";
const SITE_B_PARAM_ID: &str = "00000000-0000-4000-a000-00000000ac03";
const STREAM_A_ID: &str = "00000000-0000-4000-a000-00000000ac04";
const STREAM_B_ID: &str = "00000000-0000-4000-a000-00000000ac05";
const SENSOR_A_ID: &str = "00000000-0000-4000-a000-00000000ac06";
const SENSOR_B_ID: &str = "00000000-0000-4000-a000-00000000ac07";
const SENSOR_INVENTORY_ID: &str = "00000000-0000-4000-a000-00000000ac08";
const DEPLOYMENT_A_ID: &str = "00000000-0000-4000-a000-00000000ac09";
const DEPLOYMENT_B_ID: &str = "00000000-0000-4000-a000-00000000ac0a";
const EVENT_A_ID: &str = "00000000-0000-4000-a000-00000000ac0b";
const EVENT_B_ID: &str = "00000000-0000-4000-a000-00000000ac0c";
/// An id no row carries, for the absent-target case.
const ABSENT_ID: &str = "00000000-0000-4000-a000-00000000acff";

struct Scene {
    db: DatabaseConnection,
    app: axum::Router,
    admin: String,
}

/// Project A (the seeded one) beside project B, each with an instrument, claimable pre-deployment
/// history, uncalibrated attributed readings and one open alarm event.
async fn scene() -> Option<Scene> {
    if !kc::keycloak_reachable().await {
        eprintln!("SKIP: keycloak unreachable (start the dev stack, or set TEST_KEYCLOAK_URL)");
        return None;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let stmts = [
        format!(
            "INSERT INTO projects (id, name, description, data_source) \
             VALUES ('{PROJECT_B_ID}', 'Scope Edges B', 'second project', 'test')"
        ),
        format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m) \
             VALUES ('{SITE_B_ID}', '{PROJECT_B_ID}', 'Edges Station B', 46.0, 7.0, 500.0)"
        ),
        format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, \
                 display_units, sample_interval_sec, is_active) \
             VALUES ('{SITE_B_PARAM_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_TEMP_ID}', \
                 'Water Temperature', 'DO_Temperature', 'degC', 600, true)"
        ),
        format!(
            "INSERT INTO data_streams (id, source_system, source_key) VALUES \
             ('{STREAM_A_ID}', 'test', 'edges-a'), ('{STREAM_B_ID}', 'test', 'edges-b')"
        ),
        format!(
            "INSERT INTO sensors (id, name, is_active) VALUES \
             ('{SENSOR_A_ID}', 'edges-a', true), \
             ('{SENSOR_B_ID}', 'edges-b', true), \
             ('{SENSOR_INVENTORY_ID}', 'edges-inventory', true)"
        ),
    ];
    for sql in &stmts {
        crate::common::exec(&db, sql).await;
    }

    for (deployment, sensor, site, stream) in [
        (DEPLOYMENT_A_ID, SENSOR_A_ID, SITE1_ID, STREAM_A_ID),
        (DEPLOYMENT_B_ID, SENSOR_B_ID, SITE_B_ID, STREAM_B_ID),
    ] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO sensor_deployments \
                     (id, sensor_id, site_id, parameter_id, deployed_from, deployment_type) \
                 VALUES ('{deployment}', '{sensor}', '{site}', '{GLOBAL_PARAM_TEMP_ID}', \
                     NOW() - INTERVAL '5 days', 'permanent')"
            ),
        )
        .await;
        // Unattributed history before the deployment opens is what `backfill_candidates` reports;
        // the attributed reading with no calibration is what `calibration_candidates` reports.
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings \
                     (stream_id, time, replicate_index, site_id, parameter_id, raw_value, sensor_id) \
                 VALUES \
                     ('{stream}', NOW() - INTERVAL '10 days', 0, '{site}', '{GLOBAL_PARAM_TEMP_ID}', 1.0, NULL), \
                     ('{stream}', NOW() - INTERVAL '1 days', 0, '{site}', '{GLOBAL_PARAM_TEMP_ID}', 2.0, '{sensor}')"
            ),
        )
        .await;
    }

    for (event, site) in [(EVENT_A_ID, SITE1_ID), (EVENT_B_ID, SITE_B_ID)] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO alarm_events \
                     (id, site_id, parameter_id, severity, max_severity, started_at, \
                      value_at_start, last_seen_at, last_value) \
                 VALUES ('{event}', '{site}', '{GLOBAL_PARAM_TEMP_ID}', 2, 2, \
                     NOW() - INTERVAL '2 days', 99.0, NOW() - INTERVAL '1 hours', 99.0)"
            ),
        )
        .await;
    }

    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    Some(Scene { db, app, admin })
}

/// A fixture member at `role`, granted exactly `projects`. Fixture passwords equal the username.
async fn member(db: &DatabaseConnection, user: &str, role: &str, projects: &[&str]) -> String {
    kc::ensure_realm_user(user, user, &[role]).await;
    let sub = kc::keycloak_user_id(user).await;
    for project in projects {
        kc::grant_project(db, &sub, project).await;
    }
    kc::get_keycloak_jwt(user, user).await
}

fn site_ids(rows: &serde_json::Value) -> Vec<String> {
    rows.as_array()
        .unwrap_or_else(|| panic!("the thresholds response is an array: {rows}"))
        .iter()
        .filter_map(|r| r["site_id"].as_str().map(str::to_string))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The resolved-threshold feed
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn thresholds_carry_exactly_the_callers_projects() {
    let Some(scene) = scene().await else { return };
    let ungranted = member(&scene.db, "intern1", "riverdata-intern", &[]).await;
    let one_project = member(&scene.db, "river1", "riverdata-river", &[PROJECT_ID]).await;
    let both_projects = member(
        &scene.db,
        "manager1",
        "riverdata-manager",
        &[PROJECT_ID, PROJECT_B_ID],
    )
    .await;
    let scoped_token = crate::common::seed_api_token(
        &scene.db,
        crate::common::perms(true, true, false, false),
        Some(PROJECT_ID),
    )
    .await;

    let (status, rows) =
        crate::common::get_json_with_token(&scene.app, "/api/alarms/thresholds", &scene.admin)
            .await;
    assert_eq!(status, 200, "an administrator resolves every slot: {rows}");
    let all = site_ids(&rows);
    assert!(
        all.iter().any(|s| s == SITE1_ID) && all.iter().any(|s| s == SITE_B_ID),
        "both projects resolve a slot before scope is applied: {rows}"
    );

    let (status, rows) =
        crate::common::get_json_with_token(&scene.app, "/api/alarms/thresholds", &ungranted).await;
    assert_eq!(
        status, 200,
        "a member with no grants still reads the feed: {rows}"
    );
    assert!(
        site_ids(&rows).is_empty(),
        "a member with no grants receives no slot at all: {rows}"
    );

    for (label, caller) in [
        ("granted member", one_project.as_str()),
        ("project-scoped token", scoped_token.as_str()),
    ] {
        let (status, rows) =
            crate::common::get_json_with_token(&scene.app, "/api/alarms/thresholds", caller).await;
        assert_eq!(
            status, 200,
            "a {label} reads the resolved thresholds: {rows}"
        );
        let sites = site_ids(&rows);
        assert!(
            sites.iter().any(|s| s == SITE1_ID),
            "a {label} keeps its own project's slot: {rows}"
        );
        assert!(
            !sites.iter().any(|s| s == SITE_B_ID),
            "a {label} receives nothing from the other project: {rows}"
        );
    }

    let (status, rows) =
        crate::common::get_json_with_token(&scene.app, "/api/alarms/thresholds", &both_projects)
            .await;
    assert_eq!(
        status, 200,
        "a member granted two projects reads the feed: {rows}"
    );
    let sites = site_ids(&rows);
    assert!(
        sites.iter().any(|s| s == SITE1_ID) && sites.iter().any(|s| s == SITE_B_ID),
        "a second grant adds the second project's slots rather than replacing the first: {rows}"
    );
}

// ---------------------------------------------------------------------------------------------
// Alarm acknowledgement addressed by id
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn acknowledgement_answers_out_of_scope_and_absent_events_identically() {
    let Some(scene) = scene().await else { return };
    let manager = member(&scene.db, "manager1", "riverdata-manager", &[PROJECT_ID]).await;

    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        &format!("/api/alarms/{EVENT_A_ID}/acknowledge"),
        &json!({}),
        &manager,
    )
    .await;
    assert_eq!(
        status, 200,
        "the manager acknowledges an event in the grant: {body}"
    );

    for (label, event) in [("outside the grants", EVENT_B_ID), ("absent", ABSENT_ID)] {
        let (status, body) = crate::common::post_json_with_token(
            &scene.app,
            &format!("/api/alarms/{event}/acknowledge"),
            &json!({}),
            &manager,
        )
        .await;
        assert_eq!(status, 404, "an event {label} reads as not-found: {body}");

        let (status, body) = crate::common::delete_with_token(
            &scene.app,
            &format!("/api/alarms/{event}/acknowledge"),
            &manager,
        )
        .await;
        assert_eq!(
            status, 404,
            "un-acknowledging an event {label} reads as not-found: {body}"
        );
    }

    // An absent id is not found for an administrator either: there is nothing to acknowledge.
    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        &format!("/api/alarms/{ABSENT_ID}/acknowledge"),
        &json!({}),
        &scene.admin,
    )
    .await;
    assert_eq!(
        status, 404,
        "an absent event is not found for an administrator: {body}"
    );

    let (status, events) =
        crate::common::get_json_with_token(&scene.app, "/api/alarms/events", &scene.admin).await;
    assert_eq!(
        status, 200,
        "the administrator lists alarm events: {events}"
    );
    let row_b = events["events"]
        .as_array()
        .unwrap_or_else(|| panic!("the alarm events response carries an array: {events}"))
        .iter()
        .find(|e| e["id"] == EVENT_B_ID)
        .unwrap_or_else(|| panic!("project B's event is still listed: {events}"))
        .clone();
    assert!(
        row_b["acknowledged_by"].is_null(),
        "the refused acknowledgement stamped nobody on the other project's event: {row_b}"
    );
}

// ---------------------------------------------------------------------------------------------
// The candidate enumerations
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn candidate_enumerations_stop_at_the_grant_boundary() {
    let Some(scene) = scene().await else { return };
    let ungranted = member(&scene.db, "intern1", "riverdata-intern", &[]).await;
    let granted = member(&scene.db, "manager1", "riverdata-manager", &[PROJECT_ID]).await;

    for path in [
        "/api/actions/backfill_candidates",
        "/api/actions/calibration_candidates",
    ] {
        let (status, body) = crate::common::get_with_token(&scene.app, path, &scene.admin).await;
        assert_eq!(status, 200, "an administrator enumerates {path}: {body}");
        assert!(
            body.contains(SENSOR_A_ID) && body.contains(SENSOR_B_ID),
            "both projects produce a candidate on {path} before scope is applied: {body}"
        );

        let (status, body) = crate::common::get_with_token(&scene.app, path, &granted).await;
        assert_eq!(status, 200, "a granted member enumerates {path}: {body}");
        assert!(
            body.contains(SENSOR_A_ID),
            "the granted project's candidate survives on {path}: {body}"
        );
        assert!(
            !body.contains(SENSOR_B_ID) && !body.contains(SITE_B_ID),
            "{path} names nothing from the other project: {body}"
        );

        let (status, body) = crate::common::get_with_token(&scene.app, path, &ungranted).await;
        assert_eq!(
            status, 200,
            "a member with no grants enumerates {path}: {body}"
        );
        assert!(
            !body.contains(SENSOR_A_ID) && !body.contains(SENSOR_B_ID),
            "a member with no grants receives no candidate on {path}: {body}"
        );
    }
}

// Reprocess and rollback, targets addressed by id

#[tokio::test]
#[serial]
async fn sensor_addressed_actions_confine_by_deployment_and_admit_inventory() {
    let Some(scene) = scene().await else { return };
    let manager = member(&scene.db, "manager1", "riverdata-manager", &[PROJECT_ID]).await;

    let reprocess = |sensor: &str| json!({ "sensor_id": sensor });

    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        "/api/actions/reprocess",
        &reprocess(SENSOR_A_ID),
        &manager,
    )
    .await;
    assert_eq!(
        status, 200,
        "a manager reprocesses an instrument in the grant: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        "/api/actions/reprocess",
        &reprocess(SENSOR_B_ID),
        &manager,
    )
    .await;
    assert_eq!(
        status, 403,
        "an instrument deployed only in the other project is refused: {body}"
    );

    // An instrument with no deployment belongs to no project: it stays reachable, otherwise a
    // newly imported one would be out of reach of every member.
    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        "/api/actions/reprocess",
        &reprocess(SENSOR_INVENTORY_ID),
        &manager,
    )
    .await;
    assert_eq!(
        status, 200,
        "an instrument in inventory is reachable: {body}"
    );

    for (label, caller) in [
        ("manager", manager.as_str()),
        ("administrator", scene.admin.as_str()),
    ] {
        let (status, body) = crate::common::post_json_with_token(
            &scene.app,
            "/api/actions/reprocess",
            &reprocess(ABSENT_ID),
            caller,
        )
        .await;
        assert_eq!(
            status, 404,
            "an absent instrument is not found for a {label}: {body}"
        );
    }
}

#[tokio::test]
#[serial]
async fn rollback_confines_to_the_deployments_project() {
    let Some(scene) = scene().await else { return };
    let manager = member(&scene.db, "manager1", "riverdata-manager", &[PROJECT_ID]).await;

    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        "/api/actions/rollback_deployment",
        &json!({ "deployment_id": DEPLOYMENT_B_ID }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 403,
        "a deployment in the other project is refused: {body}"
    );

    let (status, deployment) = crate::common::get_json_with_token(
        &scene.app,
        &format!("/api/sensor_deployments/{DEPLOYMENT_B_ID}"),
        &scene.admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "the refused rollback deleted nothing: {deployment}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        "/api/actions/rollback_deployment",
        &json!({ "deployment_id": ABSENT_ID }),
        &scene.admin,
    )
    .await;
    assert_eq!(status, 404, "an absent deployment is not found: {body}");
}

// The absent-target case: an action that names nothing spans every project

#[tokio::test]
#[serial]
async fn untargeted_actions_are_refused_to_anyone_confined_to_a_project_set() {
    let Some(scene) = scene().await else { return };
    let river = member(&scene.db, "river1", "riverdata-river", &[PROJECT_ID]).await;
    let scoped_token = crate::common::seed_api_token(
        &scene.db,
        crate::common::full_permissions(),
        Some(PROJECT_ID),
    )
    .await;

    let untargeted: [(&str, serde_json::Value); 3] = [
        ("/api/actions/rebuild_alarm_events", json!({})),
        ("/api/actions/backfill_attribution", json!({ "all": true })),
        ("/api/actions/reprocess_all", json!({})),
    ];

    for (path, payload) in &untargeted {
        let (status, body) =
            crate::common::post_json_with_token(&scene.app, path, payload, &river).await;
        assert_eq!(
            status, 403,
            "{path} names nothing, so a granted member is refused: {body}"
        );

        // Control: a project-scoped token never reaches these routes at all.
        let (status, body) =
            crate::common::post_json_with_token(&scene.app, path, payload, &scoped_token).await;
        assert_eq!(
            status, 403,
            "a project-scoped token cannot call {path}: {body}"
        );

        let (status, body) =
            crate::common::post_json_with_token(&scene.app, path, payload, &scene.admin).await;
        assert_ne!(
            status, 403,
            "an administrator is unrestricted, so {path} is not a scope refusal for them: {body}"
        );
    }

    // The same actions, targeted inside the grant, are allowed.
    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        "/api/actions/rebuild_alarm_events",
        &json!({ "site_id": SITE1_ID }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "naming a site in the grant is allowed: {body}");

    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        "/api/actions/rebuild_alarm_events",
        &json!({ "site_id": SITE_B_ID }),
        &river,
    )
    .await;
    assert_eq!(
        status, 403,
        "naming a site outside the grant is refused: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        "/api/actions/backfill_attribution",
        &json!({ "deployment_ids": [DEPLOYMENT_B_ID] }),
        &river,
    )
    .await;
    assert_eq!(
        status, 403,
        "naming a deployment outside the grant is refused: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        "/api/actions/compute_derived",
        &json!({ "site_timestamps": [] }),
        &river,
    )
    .await;
    assert_eq!(
        status, 403,
        "compute_derived naming no site is refused the same way: {body}"
    );

    // The boundary: a request that asks for nothing at all is a bad request, not a scope refusal.
    let (status, body) = crate::common::post_json_with_token(
        &scene.app,
        "/api/actions/backfill_attribution",
        &json!({}),
        &river,
    )
    .await;
    assert_eq!(
        status, 400,
        "selecting nothing keeps its bad-request answer rather than becoming a scope refusal: \
         {body}"
    );
}
