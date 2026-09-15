//! `POST /sensors/proposals`: a source's own instrument register, held for a pairing plan to
//! admit. Nothing is minted here (Q134, Q178): re-offering a key refreshes the proposal it already
//! stored, a key an operator has already admitted is left alone, and a serial two register rows
//! share is carried on both, because which one claims it is decided when a plan admits them.
//!
//! Run: cargo test --test sensors instrument_proposals -- --test-threads=1

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
    Fixture {
        db: f.db,
        app: f.app,
        token: f.token,
    }
}

async fn propose(fx: &Fixture, body: serde_json::Value) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(&fx.app, "/api/sensors/proposals", &body, &fx.token)
        .await
}

fn metalp(source_key: &str, name: &str, serial: Option<&str>) -> serde_json::Value {
    let mut instrument = json!({
        "source_key": source_key,
        "name": name,
        "manufacturer": "PME",
        "model": "Cyclops-7",
        "is_lab_instrument": false,
        "metadata": {"station": "ANU", "param_name": "TURB", "in_field": 1},
    });
    if let Some(s) = serial {
        instrument["serial_number"] = json!(s);
    }
    json!({ "source_system": "metalp", "instruments": [instrument] })
}

async fn column(db: &DatabaseConnection, table: &str, key: &str, column: &str) -> Option<String> {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT ({column})::text AS v FROM {table} \
                 WHERE source_system = 'metalp' AND source_key = '{key}'"
            ),
        ))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<Option<String>>("", "v").expect("column v")
}

async fn count(db: &DatabaseConnection, table: &str) -> i64 {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT count(*) AS n FROM {table} WHERE source_system = 'metalp'"),
        ))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<i64>("", "n").expect("column n")
}

#[tokio::test]
#[serial]
async fn stores_the_register_without_minting_and_refreshes_it_on_the_next_cycle() {
    let fx = setup().await;

    let (status, body) = propose(
        &fx,
        metalp("sensor_inventory:62", "ANU TURB", Some("919402")),
    )
    .await;
    assert_eq!(status, 200, "propose: {body}");
    assert_eq!(body["stored"], 1);
    assert_eq!(body["already_admitted"], 0);
    assert_eq!(
        count(&fx.db, "sensors").await,
        0,
        "a sync mints no instrument"
    );

    let key = "sensor_inventory:62";
    assert_eq!(
        column(&fx.db, "instrument_proposals", key, "serial_number")
            .await
            .as_deref(),
        Some("919402")
    );
    assert_eq!(
        column(&fx.db, "instrument_proposals", key, "manufacturer")
            .await
            .as_deref(),
        Some("PME")
    );
    assert_eq!(
        column(&fx.db, "instrument_proposals", key, "metadata -> 'station'")
            .await
            .as_deref(),
        Some("\"ANU\"")
    );

    let (status, again) = propose(&fx, metalp(key, "ANU turbidity", Some("919402"))).await;
    assert_eq!(status, 200, "re-propose: {again}");
    assert_eq!(again["stored"], 1);
    assert_eq!(
        count(&fx.db, "instrument_proposals").await,
        1,
        "the provenance key holds one proposal"
    );
    assert_eq!(
        column(&fx.db, "instrument_proposals", key, "name")
            .await
            .as_deref(),
        Some("ANU turbidity"),
        "the register is what the source now says it is"
    );
}

#[tokio::test]
#[serial]
async fn leaves_a_key_the_operator_already_admitted_alone() {
    let fx = setup().await;
    let key = "sensor_inventory:62";

    fx.db
        .execute_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "INSERT INTO sensors (id, name, source_system, source_key, kind, data_frequency) \
                 VALUES (gen_random_uuid(), 'Bench spare', 'metalp', '{key}', 'device', 'high')"
            ),
        ))
        .await
        .expect("admit");

    let (status, body) = propose(&fx, metalp(key, "ANU TURB", Some("919402"))).await;
    assert_eq!(status, 200, "propose: {body}");
    assert_eq!(body["stored"], 0);
    assert_eq!(body["already_admitted"], 1);
    assert_eq!(count(&fx.db, "instrument_proposals").await, 0);
    assert_eq!(
        column(&fx.db, "sensors", key, "name").await.as_deref(),
        Some("Bench spare"),
        "what the operator admitted is not rewritten by a sync"
    );
}

#[tokio::test]
#[serial]
async fn carries_a_serial_two_register_rows_share() {
    let fx = setup().await;

    // METALP's register carries 919402 on two stations' turbidity probes.
    let (status, body) = propose(
        &fx,
        json!({
            "source_system": "metalp",
            "instruments": [
                {"source_key": "sensor_inventory:62", "name": "ANU TURB",
                 "serial_number": "919402", "is_lab_instrument": false},
                {"source_key": "sensor_inventory:68", "name": "FEU TURB",
                 "serial_number": "919402", "is_lab_instrument": false},
            ],
        }),
    )
    .await;
    assert_eq!(status, 200, "propose: {body}");
    assert_eq!(body["stored"], 2);

    for key in ["sensor_inventory:62", "sensor_inventory:68"] {
        assert_eq!(
            column(&fx.db, "instrument_proposals", key, "serial_number")
                .await
                .as_deref(),
            Some("919402"),
            "the register is stored as it stands; the claim is the plan's"
        );
    }
}

#[tokio::test]
#[serial]
async fn refuses_an_empty_key_or_an_unknown_field() {
    let fx = setup().await;

    let (status, body) = propose(
        &fx,
        json!({"source_system": "metalp",
               "instruments": [{"source_key": "  ", "name": "x", "is_lab_instrument": false}]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/sensors/proposals",
        &json!({"source_system": "metalp",
                "instruments": [{"source_key": "k", "name": "x",
                                 "is_lab_instrument": false, "colour": "blue"}]}),
        &fx.token,
    )
    .await;
    assert_eq!(
        status, 422,
        "an unknown field is refused, not dropped: {body}"
    );
}
