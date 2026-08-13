//! `POST /streams/register` accepting the instrument that produces a feed.
//!
//! Expected behaviour: a declared instrument is stored on the stream, so pairing reuses it instead
//! of minting a second, serial-less one; and a caller cannot name an instrument the feed has no
//! relationship to.
//!
//! The confinement rule is asserted against the guard itself rather than over HTTP: the route
//! already refuses project-scoped tokens outright, so the restricted principal the guard exists for
//! cannot be produced by a request.

use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{create_sensor as create_inventory_sensor, deploy_sensor, dt};
use crate::common::{GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID};
use river_db::common::authz::AccessScope;
use river_db::error::AppError;
use river_db::routes::private::data_streams::views::validate_declared_sensor;

async fn setup() -> (axum::Router, String, sea_orm::DatabaseConnection) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (app, token, db)
}

async fn create_sensor(app: &axum::Router, token: &str, serial: &str) -> String {
    let (status, json) = crate::common::post_json_parse_with_token(
        app,
        "/api/sensors",
        &json!({ "serial_number": serial, "manufacturer": "test", "model": "test" }),
        token,
    )
    .await;
    assert_eq!(status, 201, "create sensor ({status}): {json}");
    json["id"].as_str().expect("sensor id").to_string()
}

async fn sensor_count(db: &sea_orm::DatabaseConnection) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT count(*) AS c FROM sensors".to_string(),
    ))
    .await
    .expect("count sensors")
    .expect("one row")
    .try_get::<i64>("", "c")
    .expect("count column")
}

#[tokio::test]
#[serial]
async fn register_attaches_the_declared_instrument_and_pairing_reuses_it() {
    let (app, token, db) = setup().await;
    let sensor_id = create_sensor(&app, &token, "REG-0001").await;
    let before = sensor_count(&db).await;

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": "declare",
            "source_key": "declare-1",
            "source_name": "Declared feed",
            "sensor_id": sensor_id,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "register ({status}): {stream}");
    assert_eq!(
        stream["sensor_id"].as_str(),
        Some(sensor_id.as_str()),
        "the declared instrument is stored on the stream: {stream}"
    );

    let stream_id = stream["id"].as_str().expect("stream id");
    let (status, paired) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": crate::common::PARAM_S1_TEMP_ID }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "pair ({status}): {paired}");
    assert_eq!(
        sensor_count(&db).await,
        before,
        "pairing reuses the declared instrument instead of minting a second one"
    );
}

#[tokio::test]
#[serial]
async fn register_omitting_the_instrument_is_unchanged() {
    let (app, token, _db) = setup().await;

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({ "source_system": "declare", "source_key": "no-sensor" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "register ({status}): {stream}");
    assert!(
        stream["sensor_id"].is_null(),
        "an omitted instrument leaves the stream unattached: {stream}"
    );
}

#[tokio::test]
#[serial]
async fn register_rejects_an_instrument_that_does_not_exist() {
    let (app, token, _db) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": "declare",
            "source_key": "ghost",
            "sensor_id": "00000000-0000-4000-f000-0000000000ff",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 404, "unknown instrument ({status}): {body}");
}

#[tokio::test]
#[serial]
async fn register_rejects_an_instrument_the_metadata_contradicts() {
    let (app, token, _db) = setup().await;
    let sensor_id = create_sensor(&app, &token, "REG-0002").await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": "declare",
            "source_key": "mismatch",
            "metadata": { "device": { "logger_serial": "SOMEONE-ELSE" } },
            "sensor_id": sensor_id,
        }),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "a feed reporting another device's serial cannot claim this instrument ({status}): {body}"
    );
}

#[tokio::test]
#[serial]
async fn re_registering_is_idempotent_but_will_not_move_the_instrument() {
    let (app, token, _db) = setup().await;
    let first = create_sensor(&app, &token, "REG-0003").await;
    let second = create_sensor(&app, &token, "REG-0004").await;

    let body = |sensor: &str| {
        json!({
            "source_system": "declare",
            "source_key": "stable",
            "sensor_id": sensor,
        })
    };

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &body(&first),
        &token,
    )
    .await;
    assert_eq!(status, 200, "first register ({status}): {stream}");

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &body(&first),
        &token,
    )
    .await;
    assert_eq!(
        status, 200,
        "re-register with the same instrument: {stream}"
    );
    assert_eq!(stream["sensor_id"].as_str(), Some(first.as_str()));

    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/streams/register", &body(&second), &token)
            .await;
    assert_eq!(
        status, 409,
        "reattributing an established feed is refused ({status}): {body}"
    );
}

const OTHER_PROJECT_ID: &str = "00000000-0000-4000-a000-0000000000d1";
const OTHER_SITE_ID: &str = "00000000-0000-4000-a000-0000000000d2";

/// A project the caller below holds no grant for, to deploy an instrument into.
async fn seed_other_project(db: &sea_orm::DatabaseConnection) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) \
             VALUES ('{OTHER_PROJECT_ID}', 'Declared Other', 'second project', 'test')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m) \
             VALUES ('{OTHER_SITE_ID}', '{OTHER_PROJECT_ID}', 'Other Station', 46.0, 7.0, 500.0)"
        ),
    )
    .await;
}

fn feed_without_a_serial() -> serde_json::Value {
    json!({})
}

/// A feed that describes no device cannot contradict anything, so the serial cross-check never
/// fires and the project confinement is the whole rule. An instrument deployed nowhere belongs to
/// no project, and attaching one would resolve its calibration windows onto everything the feed
/// writes.
#[tokio::test]
#[serial]
async fn a_confined_caller_may_only_name_an_instrument_its_own_projects_deploy() {
    let (_app, _token, db) = setup().await;
    seed_other_project(&db).await;
    let scope = AccessScope::one(PROJECT_ID.parse::<Uuid>().expect("project id is a uuid"));
    let metadata = feed_without_a_serial();
    let from = dt("2025-01-01T00:00:00Z");

    let inventory = create_inventory_sensor(&db, "declared-inventory", GLOBAL_PARAM_TEMP_ID).await;
    assert!(
        matches!(
            validate_declared_sensor(&db, &scope, inventory.id, &metadata).await,
            Err(AppError::Forbidden(_))
        ),
        "an instrument deployed nowhere is not this caller's to claim"
    );

    let elsewhere = create_inventory_sensor(&db, "declared-elsewhere", GLOBAL_PARAM_TEMP_ID).await;
    deploy_sensor(&db, elsewhere.id, OTHER_SITE_ID, from).await;
    assert!(
        matches!(
            validate_declared_sensor(&db, &scope, elsewhere.id, &metadata).await,
            Err(AppError::Forbidden(_))
        ),
        "another project's instrument is refused even though the feed names no serial"
    );

    let own = create_inventory_sensor(&db, "declared-own", GLOBAL_PARAM_TEMP_ID).await;
    deploy_sensor(&db, own.id, SITE1_ID, from).await;
    assert!(
        validate_declared_sensor(&db, &scope, own.id, &metadata)
            .await
            .is_ok(),
        "an instrument deployed into the caller's own project is claimable"
    );
}

/// Wiring inventory to its first feed is the discovery case, and it stays open to the callers that
/// span projects: an administrator and an unscoped sync service.
#[tokio::test]
#[serial]
async fn an_unrestricted_caller_still_claims_undeployed_inventory() {
    let (_app, _token, db) = setup().await;
    let inventory =
        create_inventory_sensor(&db, "declared-unrestricted", GLOBAL_PARAM_TEMP_ID).await;

    assert!(
        validate_declared_sensor(
            &db,
            &AccessScope::Unrestricted,
            inventory.id,
            &feed_without_a_serial()
        )
        .await
        .is_ok(),
        "an unconfined caller reaches inventory that is deployed nowhere yet"
    );
}
