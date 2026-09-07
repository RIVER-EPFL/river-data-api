//! `POST /notes/register`: provenance-keyed idempotent upsert of a source's field notes.
//! The site comes from the source's own station name and nothing is minted for it: a note naming
//! a station river-data has never seen is reported `unresolved` and lands on a later cycle.
//!
//! Run: cargo test --test sync notes_register -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::SITE1_ID;

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
}

async fn setup() -> Fixture {
    let f = crate::common::seeded_app().await;
    Fixture { db: f.db, app: f.app, token: f.token }
}

async fn site_name(db: &DatabaseConnection) -> String {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT name FROM sites WHERE id = '{SITE1_ID}'"),
    ))
    .await
    .expect("query")
    .expect("row")
    .try_get::<String>("", "name")
    .expect("name")
}

async fn register(fx: &Fixture, notes: serde_json::Value) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/notes/register",
        &json!({"source_system": "metalp", "notes": notes}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "register ({status}): {body}");
    body
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    crate::common::e2e::count(db, sql).await
}

#[tokio::test]
#[serial]
async fn upserts_by_provenance_and_resolves_the_site_by_station_name() {
    let fx = setup().await;
    let station = site_name(&fx.db).await;

    let body = register(
        &fx,
        json!([{"source_key": "notes:1", "site_name": station, "text": "bring new pco2"}]),
    )
    .await;
    assert_eq!(body["notes"][0]["status"], "created");
    let id = body["notes"][0]["id"].as_str().expect("id").to_string();
    assert_eq!(
        count(
            &fx.db,
            &format!("SELECT count(*) FROM notes WHERE id = '{id}' AND site_id = '{SITE1_ID}' AND verified = false")
        )
        .await,
        1
    );

    // The same content again is a no-op the source may repeat every cycle.
    let body = register(
        &fx,
        json!([{"source_key": "notes:1", "site_name": station, "text": "bring new pco2"}]),
    )
    .await;
    assert_eq!(body["notes"][0]["status"], "unchanged");
    assert_eq!(body["notes"][0]["id"].as_str(), Some(id.as_str()));

    // An edit at source moves the stored note rather than adding one.
    let body = register(
        &fx,
        json!([{"source_key": "notes:1", "site_name": station, "text": "pco2 replaced", "verified": true}]),
    )
    .await;
    assert_eq!(body["notes"][0]["status"], "updated");
    assert_eq!(body["notes"][0]["id"].as_str(), Some(id.as_str()));
    assert_eq!(count(&fx.db, "SELECT count(*) FROM notes").await, 1);
    assert_eq!(
        count(
            &fx.db,
            &format!("SELECT count(*) FROM notes WHERE id = '{id}' AND verified = true AND text = 'pco2 replaced'")
        )
        .await,
        1
    );
}

#[tokio::test]
#[serial]
async fn reports_a_station_that_has_no_site_instead_of_minting_one() {
    let fx = setup().await;

    let body = register(
        &fx,
        json!([{"source_key": "notes:2", "site_name": "ZZZ", "text": "unknown station"}]),
    )
    .await;
    assert_eq!(body["notes"][0]["status"], "unresolved");
    assert!(body["notes"][0]["id"].is_null());
    assert_eq!(count(&fx.db, "SELECT count(*) FROM notes").await, 0);
    assert_eq!(
        count(&fx.db, "SELECT count(*) FROM sites WHERE name = 'ZZZ'").await,
        0,
        "a note mints no site"
    );
}
