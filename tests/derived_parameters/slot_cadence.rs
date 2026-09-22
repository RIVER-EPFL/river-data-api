//! Scenario: a site declares a calculation's output as a tool slot, and the slot says which arm
//! fills it: a person records the visit and the chain computes there (`low`), or a stream carries
//! it and the continuous engine computes there (`high`).
//!
//! Expected behaviour: the stream side reads its work off that declaration, so a low slot is no
//! stream pass's to compute and the value the chain stored at the visit instant stands. `readings`
//! holds one row per slot instant, so without the declaration the second engine to run overwrites
//! the first. The chain's own half of the gate is in
//! `src/routes/private/tools/tests/chain_applicability_tests.rs`, and what it does at a low slot
//! in the tools theme.
//!
//! Run with: cargo test --test derived_parameters slot_cadence

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use river_db::routes::private::derived_parameters::flows::site_has_active_derived;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

/// The output parameter of a one-formula calculation reading the seeded oxygen series.
async fn publish_calculation(db: &DatabaseConnection, app: &axum::Router, token: &str) -> Uuid {
    let code = format!("cadence_{}", Uuid::new_v4().simple());
    let calculation = crate::common::seed_formula_calculation(db, &format!("{code}_set")).await;
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": code,
            "name": "Cadence test mg/L",
            "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
            "tool_script_id": calculation,
        }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "{body}");
    Uuid::parse_str(body["output_parameter_id"].as_str().expect("an output")).expect("a uuid")
}

async fn declare_slot(db: &DatabaseConnection, site_id: Uuid, parameter_id: Uuid, cadence: &str) {
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO site_parameters \
           (id, site_id, parameter_id, name, sensor_type, is_active, entry_mode, cadence) \
         VALUES (gen_random_uuid(), $1, $2, 'Cadence test', 'derived', true, 'tool', $3)",
        [site_id.into(), parameter_id.into(), cadence.into()],
    ))
    .await
    .expect("the slot is declared");
}

async fn set_cadence(db: &DatabaseConnection, site_id: Uuid, parameter_id: Uuid, cadence: &str) {
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE site_parameters SET cadence = $3 WHERE site_id = $1 AND parameter_id = $2",
        [site_id.into(), parameter_id.into(), cadence.into()],
    ))
    .await
    .expect("the declaration moves");
}

#[tokio::test]
#[serial]
async fn a_low_cadence_tool_slot_is_no_work_for_the_stream_side() {
    let (db, app, token) = setup().await;
    let site_id = Uuid::parse_str(crate::common::SITE1_ID).expect("a uuid");
    let output = publish_calculation(&db, &app, &token).await;

    declare_slot(&db, site_id, output, "low").await;
    assert!(
        !site_has_active_derived(&db, site_id).await.expect("a query"),
        "a slot the lab fills at a visit is the chain's, so an ingest spawns no stream recompute"
    );

    set_cadence(&db, site_id, output, "high").await;
    assert!(
        site_has_active_derived(&db, site_id).await.expect("a query"),
        "the same slot declared on the stream arm is work the ingest must enqueue"
    );
}

/// The declaration is a column with a CHECK, not a convention each reader re-derives: a slot that
/// declares neither arm cannot be stored.
#[tokio::test]
#[serial]
async fn a_slot_declares_one_of_the_two_arms() {
    let (db, app, token) = setup().await;
    let site_id = Uuid::parse_str(crate::common::SITE1_ID).expect("a uuid");
    let output = publish_calculation(&db, &app, &token).await;

    let refused = db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO site_parameters \
               (id, site_id, parameter_id, name, sensor_type, is_active, entry_mode, cadence) \
             VALUES (gen_random_uuid(), $1, $2, 'Cadence test', 'derived', true, 'tool', 'sometimes')",
            [site_id.into(), output.into()],
        ))
        .await;
    assert!(refused.is_err(), "the CHECK is what keeps the arms two");

    declare_slot(&db, site_id, output, "low").await;
    let stored: String = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT cadence FROM site_parameters WHERE site_id = $1 AND parameter_id = $2",
            [site_id.into(), output.into()],
        ))
        .await
        .expect("a query")
        .expect("the slot")
        .try_get("", "cadence")
        .expect("a cadence");
    assert_eq!(stored, "low");
}
