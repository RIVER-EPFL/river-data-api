//! Subprojects: the mandatory project→subproject→site level. Enforcement is a DB trigger, so these
//! assert the observable contract rather than the mechanism, every project gets a default
//! subproject, every site is auto-assigned one, moving a site between subprojects keeps its project
//! consistent, and a project-scoped principal sees only its own project's subprojects.

use crate::common::fixtures::{PROJECT_ID, SITE1_ID};
use crate::common::{
    build_test_app, cleanup_test_db, exec, full_permissions, get_with_token,
    post_json_with_token, put_json_with_token, seed_api_token, seed_test_data, setup_test_db,
};
use serde_json::Value;
use serial_test::serial;

fn parse(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("bad JSON: {e}\n{body}"))
}

async fn seeded() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    let token = seed_api_token(&db, full_permissions(), None).await;
    let app = build_test_app(db.clone());
    (db, app, token)
}

#[tokio::test]
#[serial]
async fn seeding_a_project_creates_a_default_subproject() {
    let (_db, app, token) = seeded().await;
    let (s, body) = get_with_token(&app, "/api/subprojects", &token).await;
    assert_eq!(s, 200, "{body}");
    let list = parse(&body);
    let subs = list.as_array().expect("array");
    assert_eq!(subs.len(), 1, "one default subproject per project: {body}");
    assert_eq!(subs[0]["project_id"], PROJECT_ID);
    assert_eq!(subs[0]["name"], "Test River Project", "default mirrors the project name");
}

#[tokio::test]
#[serial]
async fn seeded_sites_are_auto_assigned_a_subproject() {
    let (_db, app, token) = seeded().await;
    let (s, body) = get_with_token(&app, &format!("/api/sites/{SITE1_ID}"), &token).await;
    assert_eq!(s, 200, "{body}");
    let site = parse(&body);
    assert!(
        site["subproject_id"].as_str().is_some(),
        "a site created with only a project is auto-assigned the default subproject: {body}"
    );
    assert_eq!(site["project_id"], PROJECT_ID);
}

#[tokio::test]
#[serial]
async fn moving_a_site_between_subprojects_keeps_its_project() {
    let (_db, app, token) = seeded().await;

    let (s, body) = post_json_with_token(
        &app,
        "/api/subprojects",
        &serde_json::json!({ "project_id": PROJECT_ID, "name": "North Reach" }),
        &token,
    )
    .await;
    assert_eq!(s, 201, "create subproject: {body}");
    let new_sub = parse(&body)["id"].as_str().unwrap().to_string();

    let (s, body) = put_json_with_token(
        &app,
        &format!("/api/sites/{SITE1_ID}"),
        &serde_json::json!({ "subproject_id": new_sub }),
        &token,
    )
    .await;
    assert_eq!(s, 200, "move site: {body}");

    let (_s, body) = get_with_token(&app, &format!("/api/sites/{SITE1_ID}"), &token).await;
    let site = parse(&body);
    assert_eq!(site["subproject_id"].as_str().unwrap(), new_sub, "site moved to the new subproject");
    assert_eq!(site["project_id"], PROJECT_ID, "the project is unchanged (same project)");
}

#[tokio::test]
#[serial]
async fn creating_a_site_with_a_subproject_infers_the_project() {
    let (_db, app, token) = seeded().await;

    let (_s, body) = get_with_token(&app, "/api/subprojects", &token).await;
    let default_sub = parse(&body)[0]["id"].as_str().unwrap().to_string();

    // No project_id in the body, the trigger derives it from the subproject.
    let (s, body) = post_json_with_token(
        &app,
        "/api/sites",
        &serde_json::json!({ "name": "Inferred Station", "subproject_id": default_sub }),
        &token,
    )
    .await;
    assert_eq!(s, 201, "create site by subproject: {body}");
    let site = parse(&body);
    assert_eq!(site["subproject_id"].as_str().unwrap(), default_sub);
    assert_eq!(site["project_id"], PROJECT_ID, "project inferred from the subproject");
}

#[tokio::test]
#[serial]
async fn moving_a_subproject_to_another_project_carries_its_sites() {
    let (db, app, token) = seeded().await;

    let other = "00000000-0000-4000-a000-0000000000e2";
    exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) \
             VALUES ('{other}', 'Destination Project', 'x', 'other')"
        ),
    )
    .await;

    let (s, body) = post_json_with_token(
        &app,
        "/api/subprojects",
        &serde_json::json!({ "project_id": PROJECT_ID, "name": "North Reach" }),
        &token,
    )
    .await;
    assert_eq!(s, 201, "create subproject: {body}");
    let sub = parse(&body)["id"].as_str().unwrap().to_string();

    let (s, body) = put_json_with_token(
        &app,
        &format!("/api/sites/{SITE1_ID}"),
        &serde_json::json!({ "subproject_id": sub }),
        &token,
    )
    .await;
    assert_eq!(s, 200, "assign site to subproject: {body}");

    let (s, body) = put_json_with_token(
        &app,
        &format!("/api/subprojects/{sub}"),
        &serde_json::json!({ "project_id": other }),
        &token,
    )
    .await;
    assert_eq!(s, 200, "move subproject: {body}");

    let (_s, body) = get_with_token(&app, &format!("/api/sites/{SITE1_ID}"), &token).await;
    let site = parse(&body);
    assert_eq!(site["project_id"], other, "site's project follows the moved subproject: {body}");
    assert_eq!(
        site["subproject_id"].as_str().unwrap(),
        sub,
        "site stays in the moved subproject: {body}"
    );
}

#[tokio::test]
#[serial]
async fn subproject_move_flips_scoped_visibility() {
    let (db, app, token) = seeded().await;

    let other = "00000000-0000-4000-a000-0000000000e3";
    exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) \
             VALUES ('{other}', 'Destination Project', 'x', 'other')"
        ),
    )
    .await;

    let (_s, body) = get_with_token(&app, "/api/subprojects", &token).await;
    let sub = parse(&body)
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["project_id"] == PROJECT_ID)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (s, body) = put_json_with_token(
        &app,
        &format!("/api/subprojects/{sub}"),
        &serde_json::json!({ "project_id": other }),
        &token,
    )
    .await;
    assert_eq!(s, 200, "move subproject: {body}");

    let sees_site1 = |body: &str| {
        parse(body).as_array().unwrap().iter().any(|s| s["id"] == SITE1_ID)
    };

    let dest_token = seed_api_token(&db, full_permissions(), Some(other)).await;
    let (s, body) = get_with_token(&app, "/api/sites", &dest_token).await;
    assert_eq!(s, 200, "{body}");
    assert!(sees_site1(&body), "destination-scoped principal sees the moved sites: {body}");

    let src_token = seed_api_token(&db, full_permissions(), Some(PROJECT_ID)).await;
    let (s, body) = get_with_token(&app, "/api/sites", &src_token).await;
    assert_eq!(s, 200, "{body}");
    assert!(!sees_site1(&body), "source-scoped principal no longer sees them: {body}");
}

#[tokio::test]
#[serial]
async fn scoped_principal_sees_only_its_project_subprojects() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;

    // A second project (its default subproject is created by the project trigger).
    let other = "00000000-0000-4000-a000-0000000000e1";
    exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) \
             VALUES ('{other}', 'Other Project', 'x', 'other')"
        ),
    )
    .await;

    let scoped = seed_api_token(&db, full_permissions(), Some(PROJECT_ID)).await;
    let app = build_test_app(db.clone());

    let (s, body) = get_with_token(&app, "/api/subprojects", &scoped).await;
    assert_eq!(s, 200, "{body}");
    let subs = parse(&body);
    let subs = subs.as_array().unwrap();
    assert_eq!(subs.len(), 1, "scoped token sees only its project's subproject: {body}");
    assert_eq!(subs[0]["project_id"], PROJECT_ID);
}
