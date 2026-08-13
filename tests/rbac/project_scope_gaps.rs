//! Project scope on the plain-handler routes, the half of `/api/` that the CRUD scope layers never
//! reach.
//!
//! `enforce_scope_on_crud` and `inject_read_scope` are layered on the entity router only, so every
//! hand-written handler has to confine itself and several do not. Each suite drives both kinds of
//! restricted principal, a Keycloak member granted one project and an API token confined to one
//! project, because the two are not interchangeable. `deny_scoped_token` and the `DenyScoped`
//! extractor reject a scoped TOKEN outright, while a granted member passes straight through, since
//! `access_scope` makes every non-admin member `AccessScope::Projects`. The member is therefore the
//! vector that survives the middleware, and the token assertion doubles as the control showing what
//! confinement looks like when it is present.
//!
//! Every suite also exercises a sibling route in the same source file that already confines
//! correctly, so the expectation is the codebase's own standard rather than an invention.

use axum::Router;
use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::keycloak as kc;

/// The parameter both projects carry, so a threshold or alarm row differs only by its site.
const SHARED_PARAM_CODE: &str = "RdScopeTemp";
/// Alarm ceiling on every fixture slot, and the value every fixture reading breaches it with.
const ALARM_MAX: f64 = 50.0;
const BREACH_VALUE: f64 = 99.0;

struct Scene {
    project_a: String,
    site_a: String,
    site_b: String,
    parameter: String,
}

fn days_ago(days: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

async fn fresh_db() -> sea_orm::DatabaseConnection {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    db
}

/// A Keycloak fixture user at `role`, granted visibility of exactly one project. Fixture passwords
/// equal the username.
async fn member(
    db: &sea_orm::DatabaseConnection,
    project_id: &str,
    user: &str,
    role: &str,
) -> String {
    kc::ensure_realm_user(user, user, &[role]).await;
    kc::grant_project(db, &kc::keycloak_user_id(user).await, project_id).await;
    kc::get_keycloak_jwt(user, user).await
}

/// Mint an API token through the Tokens screen's own endpoint and return its raw secret.
async fn mint_token(
    app: &Router,
    admin: &str,
    name: &str,
    permissions: serde_json::Value,
    project_scope: Option<&str>,
) -> String {
    let mut payload = json!({
        "name": name,
        "description": "project scope suite",
        "permissions": permissions,
        "created_by": "admin",
    });
    if let Some(scope) = project_scope {
        payload["project_scope"] = json!(scope);
    }
    let (status, created) =
        crate::common::post_json_parse_with_token(app, "/api/tokens", &payload, admin).await;
    assert_eq!(
        status, 201,
        "an administrator mints the {name} key: {created}"
    );
    created["token"]
        .as_str()
        .unwrap_or_else(|| panic!("create returns the raw secret exactly once: {created}"))
        .to_string()
}

fn read_only_permissions() -> serde_json::Value {
    json!({
        "read_metadata": true,
        "read_data": true,
        "write_metadata": false,
        "write_data": false,
    })
}

/// Two projects, each with one site carrying the same parameter, an alarm ceiling and a breaching
/// reading two days old. Provisioned over HTTP with an Administrator JWT, in dashboard order.
///
/// The reading is past-dated but inside the 30-day window `/alarms/thresholds` reads a current
/// value from, and it breaches the ceiling so the alarm siblings have something to confine.
async fn provision_two_projects(app: &Router, admin: &str) -> Scene {
    let project_a = e2e::create_project(app, admin, "Scope A", "rd-scope-a", true).await;
    let site_a = e2e::create_site(app, admin, &project_a, "Scope Station A", "rd-site-a").await;
    let project_b = e2e::create_project(app, admin, "Scope B", "rd-scope-b", true).await;
    let site_b = e2e::create_site(app, admin, &project_b, "Scope Station B", "rd-site-b").await;

    let parameter =
        e2e::create_parameter(app, admin, SHARED_PARAM_CODE, "Scope Temperature", "degC").await;

    for site in [&site_a, &site_b] {
        e2e::assign_site_parameter_minimal(app, admin, site, &parameter).await;

        let (status, body) = crate::common::post_json_with_token(
            app,
            "/api/alarm_thresholds",
            &json!({ "site_id": site, "parameter_id": parameter.as_str(), "alarm_max": ALARM_MAX }),
            admin,
        )
        .await;
        assert!(
            (200..300).contains(&status),
            "site {site} takes an alarm ceiling ({status}): {body}"
        );

        let (status, body) = crate::common::post_json_with_token(
            app,
            "/api/readings/batch",
            &json!({
                "readings": [{
                    "site_id": site,
                    "parameter_id": parameter.as_str(),
                    "time": days_ago(2),
                    "raw_value": BREACH_VALUE,
                    "measurement_type": "continuous",
                }]
            }),
            admin,
        )
        .await;
        assert_eq!(status, 200, "site {site} takes a breaching reading: {body}");
    }

    Scene {
        project_a,
        site_a,
        site_b,
        parameter,
    }
}

/// A slot with claimable pre-deployment history: an open deployment, unattributed readings before
/// it starts, and attributed readings inside it whose sensor carries no calibration. Returns
/// `(sensor_id, deployment_id)`.
///
/// The first shape is what `backfill_candidates` and `backfill_attribution` operate on, the second
/// what `calibration_candidates` operates on.
async fn seed_claimable_history(
    app: &Router,
    admin: &str,
    site: &str,
    parameter: &str,
    serial: &str,
) -> (String, String) {
    let sensor = e2e::create_sensor(app, admin, parameter, serial).await;
    let deployment =
        e2e::create_deployment(app, admin, &sensor, site, parameter, &days_ago(5)).await;

    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/readings/batch",
        &json!({
            "readings": [
                {
                    "site_id": site,
                    "parameter_id": parameter,
                    "time": days_ago(10),
                    "raw_value": 1.0,
                    "measurement_type": "continuous",
                },
                {
                    "site_id": site,
                    "parameter_id": parameter,
                    "time": days_ago(1),
                    "raw_value": 2.0,
                    "sensor_id": sensor.as_str(),
                    "measurement_type": "continuous",
                }
            ]
        }),
        admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "site {site} takes its claimable history: {body}"
    );

    (sensor, deployment)
}

/// `GET /api/alarms/thresholds` must confine its rows to the caller's projects, as its
/// three alarm siblings already do.
#[tokio::test]
#[serial]
async fn alarm_thresholds_confines_slots_to_the_callers_projects() {
    if !kc::require_keycloak_or_skip("alarm_thresholds_confines_slots_to_the_callers_projects")
        .await
    {
        return;
    }
    let db = fresh_db().await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let scene = provision_two_projects(&app, &admin).await;
    let manager = member(&db, &scene.project_a, "manager1", "riverdata-manager").await;
    let scoped = mint_token(
        &app,
        &admin,
        "scope-a-reader",
        read_only_permissions(),
        Some(&scene.project_a),
    )
    .await;

    // Both projects' slots really are in the unrestricted payload, so a confined response cannot
    // read as correct merely by being empty.
    let (status, body) =
        crate::common::get_with_token(&app, "/api/alarms/thresholds", &admin).await;
    assert_eq!(status, 200, "an administrator resolves every slot: {body}");
    assert!(
        body.contains(&scene.site_a) && body.contains(&scene.site_b),
        "both projects' threshold slots exist before scope is applied: {body}"
    );

    for (label, caller) in [
        ("granted member", manager.as_str()),
        ("project-scoped token", scoped.as_str()),
    ] {
        let (status, body) =
            crate::common::get_with_token(&app, "/api/alarms/active", caller).await;
        assert_eq!(status, 200, "a {label} reads the active alarm feed: {body}");
        assert!(
            body.contains(&scene.site_a),
            "the scoped sibling still returns the {label}'s own project: {body}"
        );
        assert!(
            !body.contains(&scene.site_b),
            "/alarms/active already confines to the {label}'s projects: {body}"
        );
    }

    for (label, caller) in [
        ("granted member", manager.as_str()),
        ("project-scoped token", scoped.as_str()),
    ] {
        let (status, rows) =
            crate::common::get_json_with_token(&app, "/api/alarms/thresholds", caller).await;
        assert_eq!(
            status, 200,
            "a {label} reads the resolved thresholds: {rows}"
        );
        let sites: Vec<&str> = rows
            .as_array()
            .unwrap_or_else(|| panic!("the thresholds response is an array: {rows}"))
            .iter()
            .filter_map(|r| r["site_id"].as_str())
            .collect();
        assert_eq!(
            sites,
            vec![scene.site_a.as_str()],
            "a {label} must receive its own project's threshold slot and nothing else: {rows}"
        );
    }
}

/// `merge_parameters` hard-deletes a global catalog row and `merge_site_parameters` takes
/// any project's ids, both at Manager level; the equivalent destruction through CRUD is
/// Administrator-only and project-confined.
#[tokio::test]
#[serial]
async fn parameter_merges_hold_the_administrator_and_project_gates() {
    if !kc::require_keycloak_or_skip("parameter_merges_hold_the_administrator_and_project_gates")
        .await
    {
        return;
    }
    let db = fresh_db().await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let scene = provision_two_projects(&app, &admin).await;
    let manager = member(&db, &scene.project_a, "manager1", "riverdata-manager").await;

    let source = e2e::create_parameter(
        &app,
        &admin,
        "RdScopeMergeSrc",
        "Scope Merge Source",
        "degC",
    )
    .await;
    let source_site_parameter =
        e2e::assign_site_parameter_minimal(&app, &admin, &scene.site_b, &source).await;
    let target_site_parameter = {
        let (status, body) =
            crate::common::get_json_with_token(&app, "/api/site_parameters", &admin).await;
        assert_eq!(
            status, 200,
            "the administrator lists every site parameter: {body}"
        );
        let row = body
            .as_array()
            .unwrap_or_else(|| panic!("site_parameters list is an array: {body}"))
            .iter()
            .find(|sp| {
                sp["site_id"] == scene.site_b.as_str()
                    && sp["parameter_id"] == scene.parameter.as_str()
            })
            .unwrap_or_else(|| panic!("site B carries the shared parameter: {body}"))
            .clone();
        e2e::id_of(&row)
    };

    // The global catalog is deliberately readable by every member, which is what makes the source
    // id reachable in the first place.
    let (status, body) = crate::common::get_with_token(&app, "/api/parameters", &manager).await;
    assert_eq!(status, 200, "a manager lists the global catalog: {body}");
    assert!(
        body.contains(&source),
        "the catalog read hands the manager the source parameter id: {body}"
    );

    // Control: deleting a catalog row through CRUD is Administrator-only, and that is the gate the
    // merge is expected to match.
    let (status, body) =
        crate::common::delete_with_token(&app, &format!("/api/parameters/{source}"), &manager)
            .await;
    assert_eq!(
        status, 403,
        "deleting a global parameter through CRUD is Administrator-only: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/merge_parameters",
        &json!({
            "source_parameter_id": source.as_str(),
            "target_parameter_id": scene.parameter.as_str(),
        }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 403,
        "merging a global parameter deletes the source catalog row, so it needs the same \
         Administrator gate as the CRUD delete: {body}"
    );

    let (status, body) =
        crate::common::get_with_token(&app, &format!("/api/parameters/{source}"), &admin).await;
    assert_eq!(
        status, 200,
        "the refused merge left the catalog row intact: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/merge_site_parameters",
        &json!({
            "source_site_parameter_id": source_site_parameter.as_str(),
            "target_site_parameter_id": target_site_parameter.as_str(),
        }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 403,
        "a manager granted only project A must not merge project B's site parameters: {body}"
    );

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!("/api/site_parameters/{source_site_parameter}"),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "the refused merge left project B's slot intact: {body}"
    );

    // Control: a project-scoped token is stopped outright, which is what confinement on this route
    // looks like when it is present. An unscoped `write_metadata` token is deliberately not tested:
    // the CRUD delete admits that bit too, so only the human gate differs.
    let scoped_write_token = mint_token(
        &app,
        &admin,
        "scope-a-merger",
        json!({
            "read_metadata": true,
            "read_data": true,
            "write_metadata": true,
            "write_data": true,
        }),
        Some(&scene.project_a),
    )
    .await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/merge_parameters",
        &json!({
            "source_parameter_id": source.as_str(),
            "target_parameter_id": scene.parameter.as_str(),
        }),
        &scoped_write_token,
    )
    .await;
    assert_eq!(
        status, 403,
        "a project-scoped token cannot merge the global catalog: {body}"
    );

    // The unaffected side: an Administrator performs the same merge, and it really is a catalog
    // delete, so the refusals above are refusals of destruction rather than of a no-op.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/merge_parameters",
        &json!({
            "source_parameter_id": source.as_str(),
            "target_parameter_id": scene.parameter.as_str(),
        }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "an administrator may merge the catalog ({status}): {body}"
    );
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "merge_parameters", 60).await,
        "the administrator's merge job runs to completion"
    );
    let (status, body) =
        crate::common::get_with_token(&app, &format!("/api/parameters/{source}"), &admin).await;
    assert_eq!(
        status, 404,
        "the merge deletes the source catalog row: {body}"
    );
}

/// `POST /api/actions/rollback_deployment` deletes a `sensor_deployments` row, so it must
/// need the same capability as `DELETE /api/sensor_deployments/{id}`, which is Manager /
/// `write_metadata`, not River / `write_data`.
#[tokio::test]
#[serial]
async fn rollback_deployment_needs_the_same_capability_as_deleting_the_deployment() {
    if !kc::require_keycloak_or_skip(
        "rollback_deployment_needs_the_same_capability_as_deleting_the_deployment",
    )
    .await
    {
        return;
    }
    let db = fresh_db().await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let scene = provision_two_projects(&app, &admin).await;
    let river = member(&db, &scene.project_a, "river1", "riverdata-river").await;
    let manager = member(&db, &scene.project_a, "manager1", "riverdata-manager").await;
    let write_data_token = mint_token(
        &app,
        &admin,
        "logger-write-data",
        json!({
            "read_metadata": true,
            "read_data": true,
            "write_metadata": false,
            "write_data": true,
        }),
        None,
    )
    .await;
    let scoped_write_token = mint_token(
        &app,
        &admin,
        "scope-a-writer",
        json!({
            "read_metadata": true,
            "read_data": true,
            "write_metadata": true,
            "write_data": true,
        }),
        Some(&scene.project_a),
    )
    .await;

    // Two deployments in the caller's own project: one the refused calls target, one the control
    // delete consumes, so no assertion depends on another having left the row alone.
    let probe_sensor = e2e::create_sensor(&app, &admin, &scene.parameter, "RD003-PROBE").await;
    let probe_deployment = e2e::create_deployment(
        &app,
        &admin,
        &probe_sensor,
        &scene.site_a,
        &scene.parameter,
        &days_ago(20),
    )
    .await;

    let control_parameter =
        e2e::create_parameter(&app, &admin, "RdScopeRollback", "Scope Rollback", "degC").await;
    e2e::assign_site_parameter_minimal(&app, &admin, &scene.site_a, &control_parameter).await;
    let control_sensor = e2e::create_sensor(&app, &admin, &control_parameter, "RD003-CTL").await;
    let control_deployment = e2e::create_deployment(
        &app,
        &admin,
        &control_sensor,
        &scene.site_a,
        &control_parameter,
        &days_ago(20),
    )
    .await;

    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/sensor_deployments/{probe_deployment}"),
        &river,
    )
    .await;
    assert_eq!(
        status, 403,
        "deleting a deployment through CRUD is Manager level, a River member is refused: {body}"
    );

    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/sensor_deployments/{probe_deployment}"),
        &write_data_token,
    )
    .await;
    assert_eq!(
        status, 403,
        "deleting a deployment through CRUD needs the write_metadata bit, write_data alone is \
         refused: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/rollback_deployment",
        &json!({ "deployment_id": probe_deployment.as_str() }),
        &river,
    )
    .await;
    assert_eq!(
        status, 403,
        "rollback deletes the same row, so a River member must be refused there too: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/rollback_deployment",
        &json!({ "deployment_id": probe_deployment.as_str() }),
        &write_data_token,
    )
    .await;
    assert_eq!(
        status, 403,
        "rollback deletes the same row, so a write_data-only token must be refused there too: \
         {body}"
    );

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!("/api/sensor_deployments/{probe_deployment}"),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "both refused rollbacks left the deployment in place: {body}"
    );

    // Control: a project-scoped token is already stopped by `deny_scoped_token`, which is what
    // confinement on this route looks like when it is present.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/rollback_deployment",
        &json!({ "deployment_id": probe_deployment.as_str() }),
        &scoped_write_token,
    )
    .await;
    assert_eq!(
        status, 403,
        "a project-scoped token cannot roll back a deployment: {body}"
    );

    // The unaffected side: Manager is the level that owns this destruction, through either door.
    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/sensor_deployments/{control_deployment}"),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a manager granted the project deletes a deployment through CRUD ({status}): {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/rollback_deployment",
        &json!({ "deployment_id": probe_deployment.as_str() }),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a manager granted the project rolls back a deployment ({status}): {body}"
    );
}

/// `adopt` and `retag_frequency` resolve caller-supplied site and sensor ids with no
/// project check, while their read neighbour `adopt_suggestions` confines by the same scope.
#[tokio::test]
#[serial]
async fn sensor_lifecycle_actions_refuse_another_projects_site_and_sensors() {
    if !kc::require_keycloak_or_skip(
        "sensor_lifecycle_actions_refuse_another_projects_site_and_sensors",
    )
    .await
    {
        return;
    }
    let db = fresh_db().await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let scene = provision_two_projects(&app, &admin).await;
    let manager = member(&db, &scene.project_a, "manager1", "riverdata-manager").await;
    let scoped_write_token = mint_token(
        &app,
        &admin,
        "scope-a-sensors",
        json!({
            "read_metadata": true,
            "read_data": true,
            "write_metadata": true,
            "write_data": true,
        }),
        Some(&scene.project_a),
    )
    .await;

    let sensor_a = e2e::create_sensor(&app, &admin, &scene.parameter, "RD004-A").await;
    e2e::create_deployment(
        &app,
        &admin,
        &sensor_a,
        &scene.site_a,
        &scene.parameter,
        &days_ago(20),
    )
    .await;
    let sensor_b = e2e::create_sensor(&app, &admin, &scene.parameter, "RD004-B").await;
    e2e::create_deployment(
        &app,
        &admin,
        &sensor_b,
        &scene.site_b,
        &scene.parameter,
        &days_ago(20),
    )
    .await;

    // A catalog parameter assigned nowhere, so the adopt below has a free slot at either site and
    // its id appearing in a site_parameters listing means the adopt landed.
    let adopt_parameter =
        e2e::create_parameter(&app, &admin, "RdScopeAdopt", "Scope Adopt", "degC").await;

    // Controls: the scoped reads on the same inventory already hide project B.
    let (status, body) =
        crate::common::get_with_token(&app, &format!("/api/sensors/{sensor_b}"), &manager).await;
    assert_eq!(
        status, 404,
        "project B's sensor reads as not-found for the manager: {body}"
    );

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!("/api/sensors/{sensor_b}/adopt_suggestions"),
        &manager,
    )
    .await;
    assert_eq!(
        status, 404,
        "the adopt read neighbour confines by the same scope the adopt write should: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sensors/{sensor_a}/adopt"),
        &json!({
            "site_id": scene.site_b.as_str(),
            "parameter_id": adopt_parameter.as_str(),
            "deployed_from": days_ago(10),
            "create_site_parameter": true,
        }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 403,
        "a manager granted only project A must not deploy a sensor into project B's site: {body}"
    );

    let (status, body) = crate::common::get_with_token(&app, "/api/site_parameters", &admin).await;
    assert_eq!(
        status, 200,
        "the administrator lists every site parameter: {body}"
    );
    assert!(
        !body.contains(&adopt_parameter),
        "the refused adopt created no site parameter at project B's site: {body}"
    );

    let (status, deployments) =
        crate::common::get_json_with_token(&app, "/api/sensor_deployments", &admin).await;
    assert_eq!(
        status, 200,
        "the administrator lists every deployment: {deployments}"
    );
    let at_site_b = deployments
        .as_array()
        .unwrap_or_else(|| panic!("deployments list is an array: {deployments}"))
        .iter()
        .filter(|d| d["site_id"] == scene.site_b.as_str())
        .count();
    assert_eq!(
        at_site_b, 1,
        "project B's site still carries exactly the one deployment the fixture created: \
         {deployments}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sensors/retag_frequency",
        &json!({
            "sensor_ids": [sensor_b.as_str()],
            "data_frequency": "low",
            "retag_existing": false,
        }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 403,
        "a manager granted only project A must not reclassify project B's sensor: {body}"
    );

    let (status, sensor) =
        crate::common::get_json_with_token(&app, &format!("/api/sensors/{sensor_b}"), &admin).await;
    assert_eq!(
        status, 200,
        "project B's sensor is readable by an administrator: {sensor}"
    );
    assert_eq!(
        sensor["data_frequency"], "high",
        "the refused retag left project B's sensor classification alone: {sensor}"
    );

    // Control: the scoped token is stopped outright on both writes.
    for (path, payload) in [
        (
            format!("/api/sensors/{sensor_a}/adopt"),
            json!({ "site_id": scene.site_b.as_str(), "parameter_id": adopt_parameter.as_str() }),
        ),
        (
            "/api/sensors/retag_frequency".to_string(),
            json!({ "sensor_ids": [sensor_b.as_str()], "data_frequency": "low" }),
        ),
    ] {
        let (status, body) =
            crate::common::post_json_with_token(&app, &path, &payload, &scoped_write_token).await;
        assert_eq!(
            status, 403,
            "a project-scoped token cannot call {path}: {body}"
        );
    }

    // The unaffected side: the same two writes inside the granted project still work.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sensors/{sensor_a}/adopt"),
        &json!({
            "site_id": scene.site_a.as_str(),
            "parameter_id": adopt_parameter.as_str(),
            "deployed_from": days_ago(10),
            "create_site_parameter": true,
        }),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a manager deploys a sensor inside the granted project ({status}): {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sensors/retag_frequency",
        &json!({
            "sensor_ids": [sensor_a.as_str()],
            "data_frequency": "low",
            "retag_existing": false,
        }),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a manager reclassifies a sensor inside the granted project ({status}): {body}"
    );
}

/// `backfill_candidates` and `calibration_candidates` hand every member the site, sensor
/// and deployment ids of projects they hold no grant on, while the CRUD reads of the very same
/// rows confine.
#[tokio::test]
#[serial]
async fn backfill_and_calibration_candidates_confine_to_the_callers_projects() {
    if !kc::require_keycloak_or_skip(
        "backfill_and_calibration_candidates_confine_to_the_callers_projects",
    )
    .await
    {
        return;
    }
    let db = fresh_db().await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let scene = provision_two_projects(&app, &admin).await;
    let manager = member(&db, &scene.project_a, "manager1", "riverdata-manager").await;
    let scoped = mint_token(
        &app,
        &admin,
        "scope-a-candidates",
        read_only_permissions(),
        Some(&scene.project_a),
    )
    .await;

    let (sensor_a, deployment_a) =
        seed_claimable_history(&app, &admin, &scene.site_a, &scene.parameter, "RD005-A").await;
    let (sensor_b, deployment_b) =
        seed_claimable_history(&app, &admin, &scene.site_b, &scene.parameter, "RD005-B").await;

    // Controls: the CRUD reads of the same inventory already confine.
    let (status, body) =
        crate::common::get_with_token(&app, "/api/sensor_deployments", &manager).await;
    assert_eq!(status, 200, "the manager lists deployments: {body}");
    assert!(
        body.contains(deployment_a.as_str()),
        "the manager sees the granted project's deployment: {body}"
    );
    assert!(
        !body.contains(deployment_b.as_str()),
        "the deployment read already confines to granted projects: {body}"
    );

    let (status, body) = crate::common::get_with_token(&app, "/api/sensors", &manager).await;
    assert_eq!(status, 200, "the manager lists sensors: {body}");
    assert!(
        body.contains(sensor_a.as_str()),
        "the manager sees the granted project's sensor: {body}"
    );
    assert!(
        !body.contains(sensor_b.as_str()),
        "the sensor read already confines to granted projects: {body}"
    );

    // Both projects really do produce a candidate, so an empty confined response would not pass.
    let (status, body) =
        crate::common::get_with_token(&app, "/api/actions/backfill_candidates", &admin).await;
    assert_eq!(
        status, 200,
        "the administrator enumerates backfill candidates: {body}"
    );
    assert!(
        body.contains(deployment_a.as_str()) && body.contains(deployment_b.as_str()),
        "both projects have a claimable deployment before scope is applied: {body}"
    );

    let (status, body) =
        crate::common::get_with_token(&app, "/api/actions/calibration_candidates", &admin).await;
    assert_eq!(
        status, 200,
        "the administrator enumerates calibration candidates: {body}"
    );
    assert!(
        body.contains(sensor_a.as_str()) && body.contains(sensor_b.as_str()),
        "both projects have an uncalibrated sensor before scope is applied: {body}"
    );

    let (status, body) =
        crate::common::get_with_token(&app, "/api/actions/backfill_candidates", &manager).await;
    assert_eq!(
        status, 200,
        "the manager enumerates backfill candidates: {body}"
    );
    assert!(
        body.contains(deployment_a.as_str()),
        "the manager still sees the granted project's candidate: {body}"
    );
    assert!(
        !body.contains(deployment_b.as_str()),
        "backfill candidates must not name project B's deployment: {body}"
    );
    assert!(
        !body.contains(scene.site_b.as_str()),
        "backfill candidates must not name project B's site: {body}"
    );
    assert!(
        !body.contains(sensor_b.as_str()),
        "backfill candidates must not name project B's sensor: {body}"
    );

    let (status, body) =
        crate::common::get_with_token(&app, "/api/actions/calibration_candidates", &manager).await;
    assert_eq!(
        status, 200,
        "the manager enumerates calibration candidates: {body}"
    );
    assert!(
        body.contains(sensor_a.as_str()),
        "the manager still sees the granted project's uncalibrated sensor: {body}"
    );
    assert!(
        !body.contains(sensor_b.as_str()),
        "calibration candidates must not name project B's sensor: {body}"
    );

    // Control: a project-scoped token is refused both enumerations outright.
    for path in [
        "/api/actions/backfill_candidates",
        "/api/actions/calibration_candidates",
    ] {
        let (status, body) = crate::common::get_with_token(&app, path, &scoped).await;
        assert_eq!(
            status, 403,
            "a project-scoped token cannot enumerate {path}: {body}"
        );
    }
}

/// `compute_derived`, `rebuild_alarm_events` and `backfill_attribution` enqueue work
/// against a caller-supplied site id with no grant check, while `preview_derived` in the same file
/// enforces one on the same field.
#[tokio::test]
#[serial]
async fn site_targeted_actions_refuse_a_site_outside_the_callers_grants() {
    if !kc::require_keycloak_or_skip(
        "site_targeted_actions_refuse_a_site_outside_the_callers_grants",
    )
    .await
    {
        return;
    }
    let db = fresh_db().await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let scene = provision_two_projects(&app, &admin).await;
    let river = member(&db, &scene.project_a, "river1", "riverdata-river").await;
    let scoped_write_token = mint_token(
        &app,
        &admin,
        "scope-a-actions",
        json!({
            "read_metadata": true,
            "read_data": true,
            "write_metadata": false,
            "write_data": true,
        }),
        Some(&scene.project_a),
    )
    .await;

    // `backfill_attribution` 400s when nothing is claimable, so both sites get a claimable slot and
    // the current behaviour on the out-of-scope call is a clean success rather than a bad request.
    seed_claimable_history(&app, &admin, &scene.site_a, &scene.parameter, "RD006-A").await;
    seed_claimable_history(&app, &admin, &scene.site_b, &scene.parameter, "RD006-B").await;

    let window_start = days_ago(30);
    let window_end = days_ago(0);
    let derived_at = days_ago(2);

    let payloads = |site: &str| {
        vec![
            (
                "/api/actions/rebuild_alarm_events",
                json!({ "site_id": site }),
            ),
            (
                "/api/actions/compute_derived",
                json!({
                    "site_timestamps": [{ "site_id": site, "timestamps": [derived_at.clone()] }]
                }),
            ),
            (
                "/api/actions/backfill_attribution",
                json!({ "site_id": site }),
            ),
        ]
    };

    // The unaffected side first: inside the granted project every one of the three is allowed.
    for (path, payload) in payloads(&scene.site_a) {
        let (status, body) =
            crate::common::post_json_with_token(&app, path, &payload, &river).await;
        assert!(
            (200..300).contains(&status),
            "a River member drives {path} inside the granted project ({status}): {body}"
        );
    }

    // Control: the read neighbour enforces the grant on the very same field name.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/preview_derived",
        &json!({
            "formula": "a * 2",
            "site_id": scene.site_b.as_str(),
            "start": window_start,
            "end": window_end,
        }),
        &river,
    )
    .await;
    assert_eq!(
        status, 403,
        "preview_derived already refuses a site outside the caller's grants: {body}"
    );

    for (path, payload) in payloads(&scene.site_b) {
        let (status, body) =
            crate::common::post_json_with_token(&app, path, &payload, &river).await;
        assert_eq!(
            status, 403,
            "{path} must refuse a site outside the caller's grants: {body}"
        );
    }

    // Control: a project-scoped token is stopped on all three by `deny_scoped_token`.
    for (path, payload) in payloads(&scene.site_b) {
        let (status, body) =
            crate::common::post_json_with_token(&app, path, &payload, &scoped_write_token).await;
        assert_eq!(
            status, 403,
            "a project-scoped token cannot call {path}: {body}"
        );
    }
}

fn parse_sse_frames(text: &str) -> Vec<(String, serde_json::Value)> {
    let mut frames = Vec::new();
    let mut current_event: Option<String> = None;
    for line in text.lines() {
        if let Some(event) = line.strip_prefix("event:") {
            current_event = Some(event.trim().to_string());
        } else if let Some(data) = line.strip_prefix("data:")
            && let Some(event) = current_event.take()
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(data.trim())
        {
            frames.push((event, json));
        }
    }
    frames
}

async fn open_event_stream(app: &Router, jwt: &str) -> axum::body::Body {
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/api/events")
        .header("Authorization", format!("Bearer {jwt}"))
        .header("Accept", "text/event-stream")
        .body(axum::body::Body::empty())
        .expect("event stream request builds");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("event stream responds");
    assert_eq!(
        response.status().as_u16(),
        200,
        "the event stream opens for an authenticated caller"
    );
    response.into_body()
}

/// Read SSE frames until `want` matches one or the deadline elapses, returning every frame parsed.
async fn frames_until(
    body: &mut axum::body::Body,
    secs: u64,
    want: impl Fn(&str, &serde_json::Value) -> bool,
) -> Vec<(String, serde_json::Value)> {
    use http_body_util::BodyExt;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut accumulated = String::new();
    loop {
        match tokio::time::timeout_at(deadline, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    accumulated.push_str(&String::from_utf8_lossy(data));
                    let frames = parse_sse_frames(&accumulated);
                    if frames
                        .iter()
                        .any(|(event, json)| want(event.as_str(), json))
                    {
                        return frames;
                    }
                }
            }
            Ok(Some(Err(e))) => panic!("SSE frame error: {e}"),
            Ok(None) | Err(_) => return parse_sse_frames(&accumulated),
        }
    }
}

/// `/api/events` forwards only in-scope `DataIngested` frames to any restricted principal,
/// and every non-admin Keycloak member is restricted, so a granted operator's job panel receives
/// nothing at all.
#[tokio::test]
#[serial]
async fn granted_members_receive_job_frames_on_the_event_stream() {
    if !kc::require_keycloak_or_skip("granted_members_receive_job_frames_on_the_event_stream").await
    {
        return;
    }
    let db = fresh_db().await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let scene = provision_two_projects(&app, &admin).await;
    let river = member(&db, &scene.project_a, "river1", "riverdata-river").await;

    let mut admin_stream = open_event_stream(&app, &admin).await;
    let mut river_stream = open_event_stream(&app, &river).await;

    let (status, queued) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/refresh_aggregates",
        &json!({ "full": false }),
        &river,
    )
    .await;
    assert_eq!(
        status, 200,
        "a River member triggers an aggregate refresh: {queued}"
    );
    let job_id = queued["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the refresh returns a tracked job id: {queued}"))
        .to_string();

    let matches_job = |event: &str, json: &serde_json::Value| {
        event.starts_with("job_") && json["job_id"] == job_id.as_str()
    };

    // Control: an Administrator's stream carries the job's lifecycle.
    let admin_frames = frames_until(&mut admin_stream, 60, matches_job).await;
    assert!(
        admin_frames
            .iter()
            .any(|(event, json)| matches_job(event.as_str(), json)),
        "an administrator's event stream carries the job frames: {admin_frames:?}"
    );

    // The job frame has already been broadcast, so a short read is enough to decide the member's
    // stream either has it buffered or never received it.
    let river_frames = frames_until(&mut river_stream, 10, matches_job).await;
    assert!(
        river_frames
            .iter()
            .any(|(event, json)| matches_job(event.as_str(), json)),
        "a granted member's event stream must carry the job frames it triggered: {river_frames:?}"
    );
}

/// `GET /api/sync/credentials` lists enrollment credentials at `read_metadata`, while its
/// create and revoke twins and the whole `sync_service_credentials` CRUD surface are
/// Administrator-only.
#[tokio::test]
#[serial]
async fn listing_sync_credentials_is_administrator_only() {
    if !kc::require_keycloak_or_skip("listing_sync_credentials_is_administrator_only").await {
        return;
    }
    let db = fresh_db().await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    kc::ensure_realm_user("intern1", "intern1", &["riverdata-intern"]).await;
    let intern = kc::get_keycloak_jwt("intern1", "intern1").await;
    let read_token = mint_token(&app, &admin, "read-only-key", read_only_permissions(), None).await;

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/credentials",
        &json!({ "service_type": "vaisala" }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "an administrator mints an enrollment credential ({status}): {created}"
    );
    let client_id = created["client_id"]
        .as_str()
        .unwrap_or_else(|| panic!("credential create returns a client_id: {created}"))
        .to_string();

    for (label, caller) in [
        ("read-only API token", read_token.as_str()),
        ("intern member", intern.as_str()),
    ] {
        // Control: the CRUD surface over the same table is already Administrator-only.
        let (status, body) =
            crate::common::get_with_token(&app, "/api/sync_service_credentials", caller).await;
        assert_eq!(
            status, 403,
            "the credentials CRUD surface already refuses a {label}: {body}"
        );

        let (status, body) =
            crate::common::get_with_token(&app, "/api/sync/credentials", caller).await;
        assert_eq!(
            status, 403,
            "listing enrollment credentials must be Administrator-only, a {label} is refused: \
             {body}"
        );
        assert!(
            !body.contains(&client_id),
            "a {label} must not receive the enrollment client_id: {body}"
        );
    }

    let (status, body) = crate::common::get_with_token(&app, "/api/sync/credentials", &admin).await;
    assert_eq!(
        status, 200,
        "an administrator still lists enrollment credentials: {body}"
    );
    assert!(
        body.contains(&client_id),
        "the administrator's listing carries the credential: {body}"
    );
}

/// alarm acknowledgement and the job timeline resolve by id alone, so a member granted one
/// project can act on another project's alarm event and read another project's job log, while the
/// scoped siblings that list the very same rows return 404 / omit them.
#[tokio::test]
#[serial]
async fn alarm_acknowledgement_and_job_logs_are_confined_by_project() {
    if !kc::require_keycloak_or_skip("alarm_acknowledgement_and_job_logs_are_confined_by_project")
        .await
    {
        return;
    }
    let db = fresh_db().await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let scene = provision_two_projects(&app, &admin).await;
    let manager = member(&db, &scene.project_a, "manager1", "riverdata-manager").await;
    let scoped = mint_token(
        &app,
        &admin,
        "scope-a-jobs",
        read_only_permissions(),
        Some(&scene.project_a),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/reconcile_alarms",
        &json!({}),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the administrator reconciles the breach set into alarm events ({status}): {body}"
    );

    let (status, events) =
        crate::common::get_json_with_token(&app, "/api/alarms/events", &admin).await;
    assert_eq!(
        status, 200,
        "the administrator lists alarm events: {events}"
    );
    let event_id_at = |site: &str| -> String {
        let rows = events["events"]
            .as_array()
            .unwrap_or_else(|| panic!("alarm events response carries an events array: {events}"));
        let row = rows
            .iter()
            .find(|e| e["site_id"] == site)
            .unwrap_or_else(|| panic!("an alarm event opened at {site}: {events}"));
        row["id"]
            .as_str()
            .unwrap_or_else(|| panic!("an alarm event carries an id: {row}"))
            .to_string()
    };
    let event_a = event_id_at(&scene.site_a);
    let event_b = event_id_at(&scene.site_b);

    // A tracked job per project, keyed to a sensor deployed only in that project.
    let mut jobs = Vec::new();
    for (index, site) in [&scene.site_a, &scene.site_b].into_iter().enumerate() {
        let sensor =
            e2e::create_sensor(&app, &admin, &scene.parameter, &format!("RD009-{index}")).await;
        e2e::create_deployment(&app, &admin, &sensor, site, &scene.parameter, &days_ago(20)).await;
        let (status, queued) = crate::common::post_json_parse_with_token(
            &app,
            "/api/actions/reprocess",
            &json!({ "sensor_id": sensor.as_str() }),
            &admin,
        )
        .await;
        assert_eq!(
            status, 200,
            "the administrator reprocesses the sensor: {queued}"
        );
        jobs.push(
            queued["job_id"]
                .as_str()
                .unwrap_or_else(|| panic!("reprocess returns a tracked job id: {queued}"))
                .to_string(),
        );
    }
    let (job_a, job_b) = (&jobs[0], &jobs[1]);

    // Controls: the listing and the get-by-id of the very same rows already confine.
    let (status, body) = crate::common::get_with_token(&app, "/api/alarms/events", &manager).await;
    assert_eq!(status, 200, "the manager lists alarm events: {body}");
    assert!(
        body.contains(event_a.as_str()),
        "the manager sees the granted project's alarm event: {body}"
    );
    assert!(
        !body.contains(event_b.as_str()),
        "/alarms/events already confines to the manager's projects: {body}"
    );

    for (label, caller) in [
        ("granted member", manager.as_str()),
        ("project-scoped token", scoped.as_str()),
    ] {
        let (status, body) =
            crate::common::get_with_token(&app, &format!("/api/reprocessing_jobs/{job_a}"), caller)
                .await;
        assert_eq!(
            status, 200,
            "a {label} reads a job inside its own project: {body}"
        );

        let (status, body) =
            crate::common::get_with_token(&app, &format!("/api/reprocessing_jobs/{job_b}"), caller)
                .await;
        assert_eq!(
            status, 404,
            "the job read already confines a {label} by the job's sensor project: {body}"
        );
    }

    // The unaffected side: inside the granted project both surfaces work.
    let (status, body) = crate::common::get_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{job_a}/logs"),
        &manager,
    )
    .await;
    assert_eq!(
        status, 200,
        "the manager reads a job timeline inside the grant: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/alarms/{event_a}/acknowledge"),
        &json!({}),
        &manager,
    )
    .await;
    assert_eq!(
        status, 200,
        "the manager acknowledges an alarm inside the grant: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/alarms/{event_b}/acknowledge"),
        &json!({}),
        &manager,
    )
    .await;
    assert_eq!(
        status, 404,
        "an alarm event outside the manager's grants must read as not-found: {body}"
    );

    let (status, events) =
        crate::common::get_json_with_token(&app, "/api/alarms/events", &admin).await;
    assert_eq!(
        status, 200,
        "the administrator re-reads alarm events: {events}"
    );
    let row_b = events["events"]
        .as_array()
        .unwrap_or_else(|| panic!("alarm events response carries an events array: {events}"))
        .iter()
        .find(|e| e["id"] == event_b.as_str())
        .unwrap_or_else(|| panic!("project B's alarm event is still listed: {events}"))
        .clone();
    assert!(
        row_b["acknowledged_by"].is_null(),
        "the refused acknowledgement stamped nobody on project B's event: {row_b}"
    );

    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/alarms/{event_b}/acknowledge"),
        &manager,
    )
    .await;
    assert_eq!(
        status, 404,
        "un-acknowledging an alarm event outside the manager's grants must read as not-found: \
         {body}"
    );

    for (label, caller) in [
        ("granted member", manager.as_str()),
        ("project-scoped token", scoped.as_str()),
    ] {
        let (status, body) = crate::common::get_with_token(
            &app,
            &format!("/api/reprocessing_jobs/{job_b}/logs"),
            caller,
        )
        .await;
        assert_eq!(
            status, 404,
            "a job timeline outside a {label}'s projects must read as not-found, matching the \
             job read: {body}"
        );
    }
}
