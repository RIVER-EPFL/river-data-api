//! `POST /sensors/register`: provenance-keyed idempotent upsert of a source's instrument register.
//! Re-registration resolves the same row and changes nothing on it; a serial another instrument
//! already holds is reported rather than claimed, and the registration still succeeds.
//!
//! Run: cargo test --test sensors sensor_register -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
}

async fn setup() -> Fixture {
    let f = crate::common::seeded_app().await;
    Fixture { db: f.db, app: f.app, token: f.token }
}

async fn register(fx: &Fixture, body: serde_json::Value) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(&fx.app, "/api/sensors/register", &body, &fx.token)
        .await
}

fn metalp(source_key: &str, name: &str, serial: Option<&str>) -> serde_json::Value {
    let mut body = json!({
        "source_system": "metalp",
        "source_key": source_key,
        "name": name,
        "manufacturer": "PME",
        "model": "Cyclops-7",
        "metadata": {"station": "ANU", "param_name": "TURB", "in_field": 1},
    });
    if let Some(s) = serial {
        body["serial_number"] = json!(s);
    }
    body
}

async fn column(db: &DatabaseConnection, id: &str, column: &str) -> Option<String> {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT ({column})::text AS v FROM sensors WHERE id = '{id}'"),
        ))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<Option<String>>("", "v").expect("column v")
}

#[tokio::test]
#[serial]
async fn mints_once_per_provenance_key_and_never_rewrites_a_claimed_row() {
    let fx = setup().await;

    let (status, first) = register(&fx, metalp("sensor_inventory:62", "ANU TURB", Some("919402"))).await;
    assert_eq!(status, 200, "register: {first}");
    assert_eq!(first["created"], true);
    assert!(first["serial_claimed_by"].is_null());

    let id = first["id"].as_str().expect("id").to_string();
    assert_eq!(column(&fx.db, &id, "serial_number").await.as_deref(), Some("919402"));
    assert_eq!(column(&fx.db, &id, "manufacturer").await.as_deref(), Some("PME"));
    assert_eq!(column(&fx.db, &id, "data_frequency").await.as_deref(), Some("high"));
    assert_eq!(
        column(&fx.db, &id, "metadata -> 'station'").await.as_deref(),
        Some("\"ANU\"")
    );

    // An operator renames it; the next cycle re-registers and must leave that alone.
    fx.db
        .execute_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("UPDATE sensors SET name = 'Bench spare' WHERE id = '{id}'"),
        ))
        .await
        .expect("rename");

    let (status, again) = register(&fx, metalp("sensor_inventory:62", "ANU TURB", Some("919402"))).await;
    assert_eq!(status, 200, "re-register: {again}");
    assert_eq!(again["id"], first["id"], "the provenance key resolves one instrument");
    assert_eq!(again["created"], false);
    assert_eq!(column(&fx.db, &id, "name").await.as_deref(), Some("Bench spare"));
}

#[tokio::test]
#[serial]
async fn stores_an_instrument_whose_serial_another_row_already_holds() {
    let fx = setup().await;

    let (_, first) = register(&fx, metalp("sensor_inventory:62", "ANU TURB", Some("919402"))).await;
    let first_id = first["id"].as_str().expect("id").to_string();

    // METALP's register carries 919402 on two stations' turbidity probes.
    let (status, second) =
        register(&fx, metalp("sensor_inventory:68", "FEU TURB", Some("919402"))).await;
    assert_eq!(status, 200, "second register: {second}");
    assert_eq!(second["created"], true, "the instrument is stored anyway");
    assert_eq!(
        second["serial_claimed_by"].as_str(),
        Some(first_id.as_str()),
        "the response names who holds the serial"
    );

    let second_id = second["id"].as_str().expect("id").to_string();
    assert_eq!(column(&fx.db, &second_id, "serial_number").await, None);
    assert_eq!(column(&fx.db, &first_id, "serial_number").await.as_deref(), Some("919402"));
}

#[tokio::test]
#[serial]
async fn refuses_an_empty_key_or_an_unknown_cadence() {
    let fx = setup().await;

    let (status, body) = register(
        &fx,
        json!({"source_system": "metalp", "source_key": "  ", "name": "x"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    let (status, body) = register(
        &fx,
        json!({"source_system": "metalp", "source_key": "k", "name": "x", "data_frequency": "hourly"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/sensors/register",
        &json!({"source_system": "metalp", "source_key": "k", "name": "x", "colour": "blue"}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 422, "an unknown field is refused, not dropped: {body}");
}
