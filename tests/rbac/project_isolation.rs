//! The grant (project-visibility) axis, fail-closed. A river-level user with NO grant is authenticated
//! and passes the access gate, yet sees an empty portal and is denied every write — capability alone
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
    // `user` is riverdata-user ⇒ River level, but holds no project grant.
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
