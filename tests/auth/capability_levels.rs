//! Capability matrix across the four access levels (Intern < River < Manager < Administrator).
//! Each level is a Keycloak realm role; a representative route per capability confirms the split:
//! interns read only, RIVER writes data + field metadata, MANAGER manages sensors + the catalog,
//! ADMIN alone reaches privileged surfaces. Fixture users are provisioned via the Keycloak admin
//! API; the realm roles are created out of band (see keycloak-realm-dev.json). Auto-skips when
//! Keycloak is unreachable.

//! Grants are seeded per fixture user so these tests isolate the ROLE→capability axis from the
//! project-visibility axis (a capability a level holds is only reachable inside a granted project;
//! the fail-closed "no grant ⇒ denied" behaviour is proven separately in `rbac::project_isolation`).

use crate::common::fixtures::{GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID};
use crate::common::keycloak::{
    build_test_app_with_keycloak, ensure_realm_user, get_keycloak_jwt, grant_project,
    keycloak_reachable, keycloak_user_id,
};
use sea_orm::DatabaseConnection;
use serial_test::serial;

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

fn passed_auth(status: u16) -> bool {
    status != 401 && status != 403
}

/// A minimal field-metadata write (a site note) and a catalog write (a parameter), reused so each
/// level's assertions read as a table.
fn sample_note() -> serde_json::Value {
    serde_json::json!({ "site_id": SITE1_ID, "content": "level check" })
}
fn sample_parameter(code: &str) -> serde_json::Value {
    serde_json::json!({
        "code": code, "name": "Level Check", "default_units": "x",
        "category": "measurement", "aliases": []
    })
}
fn sample_deployment() -> serde_json::Value {
    serde_json::json!({ "sensor_id": GLOBAL_PARAM_TEMP_ID, "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID })
}
fn sample_site_parameter() -> serde_json::Value {
    serde_json::json!({ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID })
}

#[tokio::test]
#[serial]
async fn intern_reads_but_cannot_write() {
    require_keycloak!();
    let (db, app) = seeded_app().await;
    ensure_realm_user("intern1", "intern1", &["riverdata-intern"]).await;
    grant_project(&db, &keycloak_user_id("intern1").await, PROJECT_ID).await;
    let jwt = get_keycloak_jwt("intern1", "intern1").await;

    let (s, _) = crate::common::get_with_token(&app, "/api/parameters", &jwt).await;
    assert_eq!(s, 200, "intern reads metadata");
    let (s, _) = crate::common::get_with_token(&app, "/api/sites", &jwt).await;
    assert_eq!(s, 200, "intern reads sites");

    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/notes", &sample_note(), &jwt).await;
    assert_eq!(s, 403, "intern cannot write field metadata");
    let batch = serde_json::json!({"readings": []});
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &batch, &jwt).await;
    assert_eq!(s, 403, "intern cannot write data");
}

#[tokio::test]
#[serial]
async fn river_writes_data_and_field_metadata_only() {
    require_keycloak!();
    let (db, app) = seeded_app().await;
    // A granted River user: `river1` holds riverdata-river and is granted the seed project, so its
    // capability (data + field metadata) is reachable. The ungranted case is `rbac::project_isolation`.
    ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    grant_project(&db, &keycloak_user_id("river1").await, PROJECT_ID).await;
    let jwt = get_keycloak_jwt("river1", "river1").await;

    let (s, body) =
        crate::common::post_json_with_token(&app, "/api/notes", &sample_note(), &jwt).await;
    assert!(
        passed_auth(s),
        "river writes field metadata (notes): {s} {body}"
    );

    let (s, _) = crate::common::post_json_with_token(
        &app,
        "/api/parameters",
        &sample_parameter("river_cap"),
        &jwt,
    )
    .await;
    assert_eq!(s, 403, "river cannot write the catalog");
    let (s, _) = crate::common::post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &sample_deployment(),
        &jwt,
    )
    .await;
    assert_eq!(s, 403, "river cannot manage sensors");
    let (s, _) = crate::common::get_with_token(&app, "/api/tokens", &jwt).await;
    assert_eq!(s, 403, "river cannot reach admin surfaces");
}

#[tokio::test]
#[serial]
async fn manager_writes_catalog_and_sensors_but_not_admin() {
    require_keycloak!();
    let (db, app) = seeded_app().await;
    ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    grant_project(&db, &keycloak_user_id("manager1").await, PROJECT_ID).await;
    let jwt = get_keycloak_jwt("manager1", "manager1").await;

    let (s, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &sample_site_parameter(),
        &jwt,
    )
    .await;
    assert!(
        passed_auth(s),
        "manager assigns parameters to sites: {s} {body}"
    );
    let (s, body) = crate::common::post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &sample_deployment(),
        &jwt,
    )
    .await;
    assert!(passed_auth(s), "manager manages sensors: {s} {body}");

    // The global parameter list is Administrator-managed, a manager cannot add a global parameter.
    let (s, _) = crate::common::post_json_with_token(
        &app,
        "/api/parameters",
        &sample_parameter("mgr_cap"),
        &jwt,
    )
    .await;
    assert_eq!(s, 403, "manager cannot write the global catalog");
    let (s, _) = crate::common::get_with_token(&app, "/api/tokens", &jwt).await;
    assert_eq!(s, 403, "manager cannot reach admin token surface");
    let (s, _) = crate::common::get_with_token(&app, "/api/api_token_audit_logs", &jwt).await;
    assert_eq!(s, 403, "manager cannot read the audit log");
}

#[tokio::test]
#[serial]
async fn admin_reaches_everything() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (s, _) = crate::common::get_with_token(&app, "/api/tokens", &jwt).await;
    assert_eq!(s, 200, "admin reaches the token surface");
    let (s, body) = crate::common::post_json_with_token(
        &app,
        "/api/parameters",
        &sample_parameter("adm_cap"),
        &jwt,
    )
    .await;
    assert!(passed_auth(s), "admin writes the catalog: {s} {body}");
}

/// A standard curve is field metadata, one level below the windowed calibration beside it.
///
/// The two are deliberately asymmetric: a calibration is sensor movement and needs MANAGER, while
/// the person entering a plate's readings adds that plate's curve in the same sitting, so RIVER is
/// enough. Both halves are asserted here so the split is a recorded decision rather than a wiring
/// accident, and an intern reads curves without being able to mint one.
#[tokio::test]
#[serial]
async fn curve_writes_follow_the_field_metadata_level() {
    require_keycloak!();
    let (db, app) = seeded_app().await;

    let sensor_id = uuid::Uuid::new_v4();
    crate::common::db::exec(
        &db,
        &format!("INSERT INTO sensors (id, name) VALUES ('{sensor_id}', 'Plate reader')"),
    )
    .await;
    let curve = |name: &str| serde_json::json!({ "sensor_id": sensor_id, "name": name, "slope": 3.0, "intercept": 0.5 });
    let calibration = serde_json::json!({
        "sensor_id": sensor_id, "slope": 1.0, "intercept": 0.0,
        "valid_from": "2026-01-01T00:00:00Z",
    });

    ensure_realm_user("intern1", "intern1", &["riverdata-intern"]).await;
    grant_project(&db, &keycloak_user_id("intern1").await, PROJECT_ID).await;
    let intern = get_keycloak_jwt("intern1", "intern1").await;

    let (s, _) = crate::common::get_with_token(&app, "/api/standard_curves", &intern).await;
    assert_eq!(s, 200, "an intern reads which curves exist");
    let (s, _) = crate::common::post_json_with_token(
        &app,
        "/api/standard_curves",
        &curve("Plate I"),
        &intern,
    )
    .await;
    assert_eq!(s, 403, "an intern cannot mint one");

    ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    grant_project(&db, &keycloak_user_id("river1").await, PROJECT_ID).await;
    let river = get_keycloak_jwt("river1", "river1").await;

    let (s, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/standard_curves",
        &curve("Plate R"),
        &river,
    )
    .await;
    assert!(
        (200..300).contains(&s),
        "a RIVER member mints the curve for the plate they are measuring: {body}"
    );
    let curve_id = body["id"]
        .as_str()
        .expect("the created curve carries an id");

    let (s, _) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/standard_curves/{curve_id}"),
        &serde_json::json!({ "notes": "eight point series" }),
        &river,
    )
    .await;
    assert!(passed_auth(s), "and annotates it");
    let (s, _) =
        crate::common::delete_with_token(&app, &format!("/api/standard_curves/{curve_id}"), &river)
            .await;
    assert!(passed_auth(s), "and removes one nothing has used");

    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/sensor_calibrations", &calibration, &river)
            .await;
    assert_eq!(
        s, 403,
        "the windowed calibration beside it stays a manager's, which is the asymmetry"
    );
}
