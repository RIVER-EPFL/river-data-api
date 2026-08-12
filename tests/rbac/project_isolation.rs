//! The grant (project-visibility) axis, fail-closed. A river-level user with NO grant is authenticated
//! and passes the access gate, yet sees an empty portal and is denied every write, capability alone
//! is not access. A manager granted project A sees and mutates only A; project B is invisible (list
//! filtered, cross-project GET 404) and unwritable (403), even though the manager level holds the
//! capability globally.

use crate::common::fixtures::{GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID};
use crate::common::keycloak::{
    build_test_app_with_keycloak, ensure_realm_user, get_keycloak_jwt, grant_project,
    keycloak_reachable, keycloak_user_id,
};
use sea_orm::DatabaseConnection;
use serial_test::serial;

const PROJECT_B_ID: &str = "00000000-0000-4000-a000-0000000000b1";
const SITE_B_ID: &str = "00000000-0000-4000-a000-0000000000b2";

macro_rules! require_keycloak {
    () => {
        if !keycloak_reachable().await {
            eprintln!("SKIP: keycloak unreachable (start the dev stack, or set TEST_KEYCLOAK_URL)");
            return;
        }
    };
}

async fn seeded_app() -> (DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = build_test_app_with_keycloak(db.clone()).await;
    (db, app)
}

/// A second project + site, outside the seed project, for cross-project isolation assertions.
async fn seed_second_project(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) VALUES \
             ('{PROJECT_B_ID}', 'Other Project', 'isolation check', 'other')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m) VALUES \
             ('{SITE_B_ID}', '{PROJECT_B_ID}', 'Other Station', 46.0, 7.0, 500.0)"
        ),
    )
    .await;
}

fn passed_auth(status: u16) -> bool {
    status != 401 && status != 403
}

#[tokio::test]
#[serial]
async fn ungranted_river_user_is_denied_everywhere() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    // `user` holds riverdata-river ⇒ River level, but has no project grant.
    let jwt = get_keycloak_jwt("user", "user").await;

    // Reads succeed at the transport layer but the scope filter empties them out.
    let (s, body) = crate::common::get_with_token(&app, "/api/sites", &jwt).await;
    assert_eq!(s, 200, "listing is allowed (scope-filtered), not rejected");
    assert!(!body.contains(SITE1_ID), "an ungranted member sees no sites: {body}");

    // Every write into a project the member isn't granted is refused.
    let note = serde_json::json!({ "site_id": SITE1_ID, "content": "x" });
    let (s, _) = crate::common::post_json_with_token(&app, "/api/notes", &note, &jwt).await;
    assert_eq!(s, 403, "ungranted member cannot write field metadata");

    let batch = serde_json::json!({
        "readings": [{ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID,
                       "time": "2024-01-01T00:00:00Z", "raw_value": 1.0 }]
    });
    let (s, _) = crate::common::post_json_with_token(&app, "/api/readings/batch", &batch, &jwt).await;
    assert_eq!(s, 403, "ungranted member cannot write data");
}

#[tokio::test]
#[serial]
async fn granted_manager_is_confined_to_granted_project() {
    require_keycloak!();
    let (db, app) = seeded_app().await;
    seed_second_project(&db).await;
    ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    grant_project(&db, &keycloak_user_id("manager1").await, PROJECT_ID).await;
    let jwt = get_keycloak_jwt("manager1", "manager1").await;

    // The granted project's sites are visible; the other project's are not.
    let (s, body) = crate::common::get_with_token(&app, "/api/sites", &jwt).await;
    assert_eq!(s, 200);
    assert!(body.contains(SITE1_ID), "granted project's sites are visible: {body}");
    assert!(!body.contains(SITE_B_ID), "the ungranted project's sites are hidden: {body}");

    // A direct fetch of an out-of-scope site is a 404 (as if it didn't exist).
    let (s, _) = crate::common::get_with_token(&app, &format!("/api/sites/{SITE_B_ID}"), &jwt).await;
    assert_eq!(s, 404, "out-of-scope site reads as not-found");

    // The manager capability is held globally, but writing into the ungranted project is refused...
    let sp_b = serde_json::json!({ "site_id": SITE_B_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID });
    let (s, _) = crate::common::post_json_with_token(&app, "/api/site_parameters", &sp_b, &jwt).await;
    assert_eq!(s, 403, "manager cannot write into an ungranted project");

    // ...while the same write into the granted project passes authorization.
    let sp_a = serde_json::json!({ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID });
    let (s, body) = crate::common::post_json_with_token(&app, "/api/site_parameters", &sp_a, &jwt).await;
    assert!(passed_auth(s), "manager writes into the granted project: {s} {body}");
}

/// H2: a `samples` row is confined by its site's project on the write side, not just on read. A
/// manager granted only project A cannot delete a sample owned by project B.
#[tokio::test]
#[serial]
async fn granted_manager_cannot_mutate_other_projects_sample() {
    require_keycloak!();
    let (db, app) = seeded_app().await;
    seed_second_project(&db).await;
    ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    grant_project(&db, &keycloak_user_id("manager1").await, PROJECT_ID).await;
    let jwt = get_keycloak_jwt("manager1", "manager1").await;

    let sample_a = "00000000-0000-4000-a000-0000000005a1";
    let sample_b = "00000000-0000-4000-a000-0000000005b1";
    for (id, site) in [(sample_a, SITE1_ID), (sample_b, SITE_B_ID)] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO samples (id, site_id, parameter_id, collected_at) \
                 VALUES ('{id}', '{site}', '{GLOBAL_PARAM_TEMP_ID}', NOW())"
            ),
        )
        .await;
    }

    let (s, _) = crate::common::delete_with_token(&app, &format!("/api/samples/{sample_b}"), &jwt).await;
    assert_eq!(s, 403, "manager cannot delete a sample in an ungranted project");

    let (s, body) = crate::common::delete_with_token(&app, &format!("/api/samples/{sample_a}"), &jwt).await;
    assert!(passed_auth(s), "manager can delete a sample in the granted project: {s} {body}");
}

/// H1: a `sensor_calibrations` row is confined by the projects its sensor is deployed to. A manager
/// granted only project A cannot patch a calibration whose sensor is deployed solely in project B,
/// editing it would rewrite project B's calibrated readings.
#[tokio::test]
#[serial]
async fn granted_manager_cannot_mutate_other_projects_calibration() {
    require_keycloak!();
    let (db, app) = seeded_app().await;
    seed_second_project(&db).await;
    ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    grant_project(&db, &keycloak_user_id("manager1").await, PROJECT_ID).await;
    let jwt = get_keycloak_jwt("manager1", "manager1").await;

    // Sensor deployed only in project B, with a calibration.
    let sensor_b = "00000000-0000-4000-a000-0000000006b1";
    let cal_b = "00000000-0000-4000-a000-0000000006b2";
    // Sensor deployed in project A (the granted one), with its own calibration.
    let sensor_a = "00000000-0000-4000-a000-0000000006a1";
    let cal_a = "00000000-0000-4000-a000-0000000006a2";
    for (sensor, site, cal) in [(sensor_b, SITE_B_ID, cal_b), (sensor_a, SITE1_ID, cal_a)] {
        crate::common::exec(
            &db,
            &format!("INSERT INTO sensors (id, name, is_active) VALUES ('{sensor}', 'cal-{sensor}', true)"),
        )
        .await;
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO sensor_deployments (id, sensor_id, site_id, parameter_id, deployed_from, deployment_type) \
                 VALUES (gen_random_uuid(), '{sensor}', '{site}', '{GLOBAL_PARAM_TEMP_ID}', NOW() - INTERVAL '2 days', 'permanent')"
            ),
        )
        .await;
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO sensor_calibrations (id, sensor_id, parameter_id, slope, intercept, valid_from) \
                 VALUES ('{cal}', '{sensor}', '{GLOBAL_PARAM_TEMP_ID}', 2.0, 0.0, NOW() - INTERVAL '2 days')"
            ),
        )
        .await;
    }

    let patch = serde_json::json!({ "slope": 3.0 });
    let (s, _) =
        crate::common::patch_json_with_token(&app, &format!("/api/sensor_calibrations/{cal_b}"), &patch, &jwt).await;
    assert_eq!(s, 403, "manager cannot patch a calibration for a sensor deployed only in project B");

    let (s, body) =
        crate::common::patch_json_with_token(&app, &format!("/api/sensor_calibrations/{cal_a}"), &patch, &jwt).await;
    assert!(passed_auth(s), "manager can patch a calibration for a sensor in the granted project: {s} {body}");
}

/// H3: creating a site by omitting `project_id` and naming only a `subproject_id` must not bypass the
/// scope guard, the DB trigger would otherwise stamp the site into the subproject's project. A
/// manager granted only A cannot create a site under project B's subproject.
#[tokio::test]
#[serial]
async fn granted_manager_cannot_create_site_via_other_projects_subproject() {
    require_keycloak!();
    let (db, app) = seeded_app().await;
    seed_second_project(&db).await;
    ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    grant_project(&db, &keycloak_user_id("manager1").await, PROJECT_ID).await;
    let jwt = get_keycloak_jwt("manager1", "manager1").await;

    let sub_a = "00000000-0000-4000-a000-0000000007a1";
    let sub_b = "00000000-0000-4000-a000-0000000007b1";
    crate::common::exec(
        &db,
        &format!("INSERT INTO subprojects (id, project_id, name) VALUES ('{sub_a}', '{PROJECT_ID}', 'SubA')"),
    )
    .await;
    crate::common::exec(
        &db,
        &format!("INSERT INTO subprojects (id, project_id, name) VALUES ('{sub_b}', '{PROJECT_B_ID}', 'SubB')"),
    )
    .await;

    // No project_id in the body, only a subproject that belongs to the ungranted project B.
    let sneaky = serde_json::json!({ "name": "Sneaky Station", "subproject_id": sub_b });
    let (s, _) = crate::common::post_json_with_token(&app, "/api/sites", &sneaky, &jwt).await;
    assert_eq!(s, 403, "omitting project_id must not let a member create a site inside project B");

    // The same shape targeting the granted project's subproject passes authorization.
    let ok = serde_json::json!({ "name": "Legit Station", "subproject_id": sub_a });
    let (s, body) = crate::common::post_json_with_token(&app, "/api/sites", &ok, &jwt).await;
    assert!(passed_auth(s), "member can create a site under a granted project's subproject: {s} {body}");
}
