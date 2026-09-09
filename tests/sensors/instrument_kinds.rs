//! What an instrument row is, on the row.
//!
//! Three of the four minting paths produce something that is not a device: the instrument a source
//! carries for one parameter across every station, the per-slot entry channel behind a hand-typed
//! value, and the pseudo-instrument a portal curve label mints. `(source_system, source_key)` stays
//! the identity; `kind` is what a picker and the inventory read to tell a spectrophotometer from a
//! bookkeeping row.
//!
//! Run: cargo test --test sensors instrument_kinds -- --test-threads=1

use sea_orm::{ConnectionTrait, EntityTrait, Statement};
use serde_json::json;
use uuid::Uuid;
use serial_test::serial;

async fn kind_of(db: &sea_orm::DatabaseConnection, sensor_id: &str) -> String {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT kind FROM sensors WHERE id = '{sensor_id}'"),
        ))
        .await
        .expect("query")
        .expect("the instrument exists");
    row.try_get::<String>("", "kind").expect("kind")
}

async fn frequency_of(db: &sea_orm::DatabaseConnection, sensor_id: &str) -> String {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT data_frequency FROM sensors WHERE id = '{sensor_id}'"),
        ))
        .await
        .expect("query")
        .expect("the instrument exists");
    row.try_get::<String>("", "data_frequency")
        .expect("data_frequency")
}

/// Register a feed and mint its instrument the way the pairing does. Registration itself attaches
/// none, so this is what stands in for the pairing in a test about which kind each path stamps.
async fn mint_for_feed(
    db: &sea_orm::DatabaseConnection,
    app: &axum::Router,
    token: &str,
    source_system: &str,
    source_key: &str,
    metadata: serde_json::Value,
) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({
            "source_system": source_system,
            "source_key": source_key,
            "metadata": metadata,
        }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "register ({status}): {stream}");
    assert!(
        stream["sensor_id"].is_null(),
        "registration mints nothing: {stream}"
    );
    let id: Uuid = stream["id"].as_str().expect("stream id").parse().expect("uuid");
    let model = river_db::routes::private::data_streams::Entity::find_by_id(id)
        .one(db)
        .await
        .expect("query stream")
        .expect("the stream exists");
    river_db::routes::private::sensors::identity::resolve_or_mint_stream_instrument(
        db,
        &model,
        None,
        river_db::routes::private::sensors::identity::InstrumentKind::SourceParameter,
    )
    .await
    .expect("mint the instrument")
    .to_string()
}

#[tokio::test]
#[serial]
async fn each_minting_path_stamps_what_it_made() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    // Registration mints nothing (M172), so both feed paths are exercised through the function the
    // pairing and the plan apply mint with.
    // A device feed: the channel is the instrument.
    let device = mint_for_feed(
        &db,
        &app,
        &token,
        "vaisala",
        "kinds-device",
        json!({"device": {"logger_serial": "LOG-1"}}),
    )
    .await;
    assert_eq!(kind_of(&db, &device).await, "device");

    // A feed that describes no device: one instrument per parameter across every station.
    let source_parameter =
        mint_for_feed(&db, &app, &token, "cnet", "FP1:DOC_avg_ppb", json!({})).await;
    assert_eq!(kind_of(&db, &source_parameter).await, "source_parameter");
    // A bookkeeping row carries no evidence about cadence, and `data_frequency` is read as one by
    // the measurement-type chain, so 'high' is what keeps that rung silent.
    assert_eq!(
        frequency_of(&db, &source_parameter).await,
        "high",
        "a bookkeeping instrument classifies nothing"
    );

    // A hand-entered value: the slot's own entry channel, never the deployed probe.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": crate::common::SITE1_ID,
            "readings": [{
                "parameter_id": crate::common::GLOBAL_PARAM_TURB_ID,
                "value": 4.0,
                "time": "2025-06-10T08:00:00Z",
            }],
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "grab save ({status}): {body}");
    let entry = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT r.sensor_id::text AS id FROM readings r \
                 WHERE r.site_id = '{}' AND r.parameter_id = '{}' \
                   AND r.measurement_type = 'spot'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TURB_ID
            ),
        ))
        .await
        .expect("query")
        .expect("the grab reading names an instrument")
        .try_get::<String>("", "id")
        .expect("sensor id");
    assert_eq!(kind_of(&db, &entry).await, "entry_channel");

    // A portal curve label: not an instrument at all, one row per analyte.
    let (status, curve) = crate::common::post_json_parse_with_token(
        &app,
        "/api/standard_curves/register",
        &json!({
            "source_system": "cnet",
            "source_key": "standard_curves:9",
            "instrument_label": "DOC corr",
            "slope": 2.0,
            "intercept": 1.0,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "curve ({status}): {curve}");
    let lab = curve["sensor_id"].as_str().expect("curve instrument");
    assert_eq!(kind_of(&db, lab).await, "lab");
}

/// Expected behaviour: an instrument created by hand is a device until something says otherwise,
/// and the list can be narrowed to the kinds a picker should offer.
#[tokio::test]
#[serial]
async fn the_inventory_separates_the_kinds() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, sensor) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensors",
        &json!({"serial_number": "KIND-1", "manufacturer": "Test"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {sensor}");
    assert_eq!(sensor["kind"], "device", "{sensor}");

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "FP2:NO2_mgL"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "register ({status}): {stream}");

    let filter = crate::common::e2e::percent_encode(r#"{"kind":"device"}"#);
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensors?filter={filter}&page=1&per_page=100"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "list ({status}): {body}");
    let ids: Vec<&str> = body
        .as_array()
        .expect("array body")
        .iter()
        .map(|s| s["id"].as_str().unwrap_or_default())
        .collect();
    assert!(
        ids.contains(&sensor["id"].as_str().unwrap()),
        "the hand-created instrument is a device: {body}"
    );
    // Registration mints nothing, so there is no bookkeeping row to exclude yet: minting one the
    // way the pairing does is what puts it in the inventory, and the filter still leaves it out.
    let bookkeeping = mint_for_feed(&db, &app, &token, "cnet", "FP2:NO2_mgL:minted", json!({})).await;
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensors?filter={filter}&page=1&per_page=100"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "list ({status}): {body}");
    let ids: Vec<&str> = body
        .as_array()
        .expect("array body")
        .iter()
        .map(|s| s["id"].as_str().unwrap_or_default())
        .collect();
    assert!(
        !ids.contains(&bookkeeping.as_str()),
        "the minted bookkeeping row is not offered as one: {body}"
    );

    // The inventory opens on what something was measured on, which is one filter over two kinds.
    let both = crate::common::e2e::percent_encode(r#"{"kind":["device","lab"]}"#);
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensors?filter={both}&page=1&per_page=100"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "list ({status}): {body}");
    let kinds: Vec<&str> = body
        .as_array()
        .expect("array body")
        .iter()
        .map(|s| s["kind"].as_str().unwrap_or_default())
        .collect();
    assert!(!kinds.is_empty(), "the pair of kinds returns rows: {body}");
    assert!(
        kinds.iter().all(|k| *k == "device" || *k == "lab"),
        "only the measuring kinds come back: {body}"
    );
}
