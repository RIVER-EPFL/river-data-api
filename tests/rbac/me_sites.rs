//! `GET /api/me/sites`, the sidebar navigator's project → subproject → site tree. Visibility
//! follows the grant axis exactly like `/api/sites`: an administrator gets every project, a granted
//! member gets only their project's tree, an ungranted member gets an empty list. Sites land under
//! the subproject the row points at (the default subproject when none was given at insert).

use crate::common::fixtures::{PROJECT_ID, SITE1_ID, SITE2_ID};
use crate::common::keycloak::{
    build_test_app_with_keycloak, ensure_realm_user, get_keycloak_jwt, grant_project,
    keycloak_reachable, keycloak_user_id,
};
use sea_orm::DatabaseConnection;
use serial_test::serial;

const PROJECT_B_ID: &str = "00000000-0000-4000-a000-0000000000b1";
const SITE_B_ID: &str = "00000000-0000-4000-a000-0000000000b2";
const SUBPROJECT_TRIB_ID: &str = "00000000-0000-4000-a000-0000000000c1";
const SITE_TRIB_ID: &str = "00000000-0000-4000-a000-0000000000c2";

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

/// A named (non-default) subproject in the seed project, with one site assigned to it.
async fn seed_named_subproject(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO subprojects (id, project_id, name) VALUES \
             ('{SUBPROJECT_TRIB_ID}', '{PROJECT_ID}', 'Tributaries')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO sites (id, project_id, subproject_id, name, latitude, longitude, altitude_m) \
             VALUES ('{SITE_TRIB_ID}', '{PROJECT_ID}', '{SUBPROJECT_TRIB_ID}', 'Tributary Station', \
             51.6, -0.2, 20.0)"
        ),
    )
    .await;
}

fn tree(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("me/sites returns JSON")
}

fn site_ids(project: &serde_json::Value) -> Vec<String> {
    project["subprojects"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|sp| sp["sites"].as_array().unwrap().iter())
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
#[serial]
async fn admin_sees_every_project_tree() {
    require_keycloak!();
    let (db, app) = seeded_app().await;
    seed_second_project(&db).await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (s, body) = crate::common::get_with_token(&app, "/api/me/sites", &jwt).await;
    assert_eq!(s, 200, "{body}");
    let projects = tree(&body);
    let names: Vec<&str> = projects
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"Test River Project"),
        "admin sees the seed project: {body}"
    );
    assert!(
        names.contains(&"Other Project"),
        "admin sees the second project: {body}"
    );

    let other = projects
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Other Project")
        .unwrap();
    assert!(
        site_ids(other).contains(&SITE_B_ID.to_string()),
        "sites listed per project: {body}"
    );
}

#[tokio::test]
#[serial]
async fn granted_member_gets_only_their_project_tree() {
    require_keycloak!();
    let (db, app) = seeded_app().await;
    seed_second_project(&db).await;
    ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    grant_project(&db, &keycloak_user_id("manager1").await, PROJECT_ID).await;
    let jwt = get_keycloak_jwt("manager1", "manager1").await;

    let (s, body) = crate::common::get_with_token(&app, "/api/me/sites", &jwt).await;
    assert_eq!(s, 200, "{body}");
    let projects = tree(&body);
    assert_eq!(
        projects.as_array().unwrap().len(),
        1,
        "only the granted project: {body}"
    );
    let project = &projects[0];
    assert_eq!(project["project_id"], PROJECT_ID);
    let ids = site_ids(project);
    assert!(
        ids.contains(&SITE1_ID.to_string()) && ids.contains(&SITE2_ID.to_string()),
        "{body}"
    );
    assert!(
        !body.contains(SITE_B_ID),
        "ungranted project's sites are invisible: {body}"
    );
}

#[tokio::test]
#[serial]
async fn ungranted_member_gets_empty_tree() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("user", "user").await;

    let (s, body) = crate::common::get_with_token(&app, "/api/me/sites", &jwt).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        tree(&body),
        serde_json::json!([]),
        "no grants means an empty navigator: {body}"
    );
}

#[tokio::test]
#[serial]
async fn sites_group_under_their_subproject() {
    require_keycloak!();
    let (db, app) = seeded_app().await;
    seed_named_subproject(&db).await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (s, body) = crate::common::get_with_token(&app, "/api/me/sites", &jwt).await;
    assert_eq!(s, 200, "{body}");
    let projects = tree(&body);
    let project = projects
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["project_id"] == PROJECT_ID)
        .unwrap();

    let subprojects = project["subprojects"].as_array().unwrap();
    let named = subprojects
        .iter()
        .find(|sp| sp["name"] == "Tributaries")
        .unwrap();
    let named_sites: Vec<&str> = named["sites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(named_sites, vec![SITE_TRIB_ID], "{body}");

    // The seed sites (inserted without a subproject) sit under the trigger-derived default, not
    // alongside the named subproject's site.
    let default = subprojects
        .iter()
        .find(|sp| sp["name"] == "Test River Project")
        .expect("default group");
    let default_ids: Vec<&str> = default["sites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert!(
        default_ids.contains(&SITE1_ID) && default_ids.contains(&SITE2_ID),
        "{body}"
    );
}
