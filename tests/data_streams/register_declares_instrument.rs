//! `POST /streams/register` accepting the instrument that produces a feed.
//!
//! Expected behaviour: a declared instrument is stored on the stream, so pairing reuses it instead
//! of minting a second, serial-less one; and a caller cannot name an instrument the feed has no
//! relationship to.

use serde_json::json;
use serial_test::serial;

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
