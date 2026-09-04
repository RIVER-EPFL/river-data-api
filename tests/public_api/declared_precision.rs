//! A sync service declares the source's decimal places on the stream descriptor, pairing writes
//! that onto the slot where it has none, and the public arm expresses every served value at the
//! slot's declared places while the private arm serves the stored double. A slot with no
//! declaration is served unrounded (`export_double_roundtrip`).
//!
//! Run: cargo test --test public_api declared_precision -- --test-threads=1

use serde_json::value::RawValue;
use serial_test::serial;
use uuid::Uuid;

const T0: &str = "2025-06-01T00:00:00Z";
const WINDOW: &str = "start=2025-06-01T00:00:00Z&end=2025-06-01T01:00:00Z";
const PUBLIC_URI: &str = "/api/public/test-river/sites/upstream/readings";

/// A MySQL single-precision 100.8 widened to a double.
fn stored() -> f64 {
    f64::from(100.8_f32)
}

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE sites SET public_code = 'upstream' WHERE id = '{}'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_public = true, decimal_places = NULL WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn slot_decimal_places(db: &sea_orm::DatabaseConnection, slot: &str) -> Option<i16> {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT decimal_places AS v FROM site_parameters WHERE id = '{slot}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<Option<i16>>("", "v")
    .unwrap()
}

/// Register a stream declaring two decimal places and pair it into the temperature slot.
async fn register_and_pair(app: &axum::Router, token: &str) -> Uuid {
    let (status, json) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &serde_json::json!({
            "source_system": "cnet",
            "source_key": "VAD:DOC_rep_1",
            "source_name": "VAD - DOC_rep_1",
            "source_path": "cnet/VAD/DOC_rep_1",
            "measurement_type": "continuous",
            "decimal_places": 2
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "register: {json}");
    let stream_id: Uuid = json["id"].as_str().unwrap().parse().unwrap();

    let (status, text) = crate::common::post_json_with_token(
        app,
        &format!("/api/streams/{stream_id}/pair"),
        &serde_json::json!({ "site_parameter_id": crate::common::PARAM_S1_TEMP_ID }),
        token,
    )
    .await;
    assert_eq!(status, 200, "pair: {text}");
    stream_id
}

fn parse_std(text: &str) -> f64 {
    text.trim()
        .parse()
        .unwrap_or_else(|e| panic!("parse f64 from {text:?}: {e}"))
}

#[tokio::test]
#[serial]
async fn register_refuses_an_out_of_range_declaration() {
    let (_db, app, token) = setup().await;
    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({
            "source_system": "cnet",
            "source_key": "VAD:bad",
            "decimal_places": 11
        }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "{text}");
}

#[tokio::test]
#[serial]
async fn pairing_writes_the_declared_places_onto_an_undeclared_slot() {
    let (db, app, token) = setup().await;
    assert_eq!(
        slot_decimal_places(&db, crate::common::PARAM_S1_TEMP_ID).await,
        None
    );
    register_and_pair(&app, &token).await;
    assert_eq!(
        slot_decimal_places(&db, crate::common::PARAM_S1_TEMP_ID).await,
        Some(2),
        "the stream's declaration reaches the slot"
    );
}

#[tokio::test]
#[serial]
async fn pairing_leaves_a_slot_that_already_declares() {
    let (db, app, token) = setup().await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET decimal_places = 4 WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;
    register_and_pair(&app, &token).await;
    assert_eq!(
        slot_decimal_places(&db, crate::common::PARAM_S1_TEMP_ID).await,
        Some(4),
        "an operator's declaration is not overwritten by the source's"
    );
}

#[tokio::test]
#[serial]
async fn the_public_arm_rounds_and_the_private_arm_is_bit_exact() {
    let (db, app, token) = setup().await;
    let stream_id = register_and_pair(&app, &token).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                raw_value, measurement_type) \
             VALUES ('{stream_id}', '{site}', '{param}', '{T0}', 0, {value:?}, 'continuous')",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TEMP_ID,
            value = stored(),
        ),
    )
    .await;

    #[derive(serde::Deserialize)]
    struct Resp<'a> {
        #[serde(borrow)]
        parameters: Vec<ParamOut<'a>>,
    }
    #[derive(serde::Deserialize)]
    struct ParamOut<'a> {
        #[serde(borrow)]
        values: Vec<Option<&'a RawValue>>,
    }

    let (status, body) = crate::common::get(&app, &format!("{PUBLIC_URI}?{WINDOW}")).await;
    assert_eq!(status, 200, "public json: {body}");
    let resp: Resp = serde_json::from_str(&body).unwrap();
    let raw = resp.parameters[0].values[0].unwrap().get();
    assert_eq!(
        raw, "100.8",
        "public JSON expresses the declared places: {body}"
    );

    let (status, csv) =
        crate::common::get(&app, &format!("{PUBLIC_URI}?{WINDOW}&format=csv")).await;
    assert_eq!(status, 200, "public csv: {csv}");
    let row = csv
        .lines()
        .nth(1)
        .unwrap_or_else(|| panic!("data row: {csv}"));
    assert_eq!(row.split(',').nth(1).unwrap(), "100.8", "public CSV: {csv}");

    let (status, ndjson) =
        crate::common::get(&app, &format!("{PUBLIC_URI}?{WINDOW}&format=ndjson")).await;
    assert_eq!(status, 200, "public ndjson: {ndjson}");
    let line: serde_json::Value = serde_json::from_str(ndjson.lines().next().unwrap()).unwrap();
    assert_eq!(line["DO_Temperature"], 100.8, "public NDJSON: {ndjson}");

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{}/readings?{WINDOW}&parameter_ids={}",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "private json: {body}");
    let resp: Resp = serde_json::from_str(&body).unwrap();
    let raw = resp.parameters[0].values[0].unwrap().get();
    assert_eq!(
        parse_std(raw).to_bits(),
        stored().to_bits(),
        "the private arm serves the stored double: {raw}"
    );

    let (status, body) = crate::common::get_json(
        &app,
        &format!("/api/public/test-river/sites/upstream/parameters"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body[0].get("decimal_places").is_none(),
        "decimal_places is not published: {body}"
    );
}

#[tokio::test]
#[serial]
async fn the_public_aggregates_express_the_declared_places() {
    let (db, app, token) = setup().await;
    let stream_id = register_and_pair(&app, &token).await;
    for (minute, value) in [(0, stored()), (10, f64::from(100.9_f32))] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                    raw_value, measurement_type) \
                 VALUES ('{stream_id}', '{site}', '{param}', '2025-06-01T00:{minute:02}:00Z', 0, \
                         {value:?}, 'continuous')",
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
    }
    crate::common::exec(
        &db,
        "CALL refresh_continuous_aggregate('readings_hourly', '2025-05-31', '2025-06-02')",
    )
    .await;

    let (status, body) = crate::common::get_json(
        &app,
        &format!("/api/public/test-river/sites/upstream/aggregates/hourly?{WINDOW}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let param = &body["parameters"][0];
    assert_eq!(param["avg"][0], 100.85, "{body}");
    assert_eq!(param["min"][0], 100.8, "{body}");
    assert_eq!(param["max"][0], 100.9, "{body}");
}
