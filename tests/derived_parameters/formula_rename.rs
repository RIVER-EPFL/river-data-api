//! A formula reads another of its set by code, so renaming one carries into every formula that
//! reads it, and a reader left naming a formula the set dropped is refused rather than bound to the
//! catalog parameter of that name (B605).
//!
//! Run with: cargo test --test derived_parameters formula_rename

use serde_json::{Value, json};
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn calculation(db: &sea_orm::DatabaseConnection, name: &str) -> String {
    let id = uuid::Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO tool_scripts (id, name, label, engine) \
             VALUES ('{id}', '{name}', '{name}', 'formula')"
        ),
    )
    .await;
    id.to_string()
}

async fn save(app: &axum::Router, token: &str, calculation: &str, body: &Value) -> (u16, Value) {
    crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{calculation}/formulas"),
        body,
        token,
    )
    .await
}

async fn stored(db: &sea_orm::DatabaseConnection, code: &str) -> (String, String) {
    let id = crate::common::e2e::scalar(
        db,
        &format!("SELECT id::text FROM calculation_formulas WHERE code = '{code}'"),
    )
    .await;
    let formula = crate::common::e2e::scalar(
        db,
        &format!("SELECT formula FROM calculation_formulas WHERE code = '{code}'"),
    )
    .await;
    (id, formula)
}

/// Two outputs, the second reading the first per replicate.
async fn headspace_set(
    db: &sea_orm::DatabaseConnection,
    app: &axum::Router,
    token: &str,
) -> String {
    let calc = calculation(db, "headspace").await;
    let (status, body) = save(
        app,
        token,
        &calc,
        &json!({ "formulas": [
            { "code": "hs_um", "name": "HS", "units": "uM", "formula": "Dissolved_O2 * 2", "ordinal": 0 },
            { "code": "hs_uatm", "name": "HS uatm", "units": "uatm", "formula": "hs_um / 3",
              "ordinal": 1, "per_replicate": "hs_um" },
        ]}),
    )
    .await;
    assert_eq!(status, 200, "the set saves: {body}");
    calc
}

#[tokio::test]
#[serial]
async fn renaming_a_formula_carries_into_the_formula_reading_it() {
    let (db, app, token) = setup().await;
    let calc = headspace_set(&db, &app, &token).await;
    let (hs, _) = stored(&db, "hs_um").await;
    let (uatm, _) = stored(&db, "hs_uatm").await;

    // The rename posted as an API client would, with the reader's text untouched.
    let (status, body) = save(
        &app,
        &token,
        &calc,
        &json!({ "formulas": [
            { "id": hs, "code": "hs_um2", "name": "HS", "units": "uM", "formula": "Dissolved_O2 * 2", "ordinal": 0 },
            { "id": uatm, "code": "hs_uatm", "name": "HS uatm", "units": "uatm", "formula": "hs_um / 3",
              "ordinal": 1, "per_replicate": "hs_um" },
        ]}),
    )
    .await;
    assert_eq!(status, 200, "the rename saves: {body}");

    let (_, formula) = stored(&db, "hs_uatm").await;
    assert_eq!(formula, "hs_um2 / 3");
    let over = crate::common::e2e::scalar(
        &db,
        "SELECT per_replicate FROM calculation_formulas WHERE code = 'hs_uatm'",
    )
    .await;
    assert_eq!(over, "hs_um2");
    let sources = crate::common::e2e::scalar(
        &db,
        &format!(
            "SELECT COUNT(*)::text FROM derived_parameter_sources WHERE derived_definition_id = '{uatm}' \
             AND variable_name = 'hs_um'"
        ),
    )
    .await;
    assert_eq!(sources, "0", "nothing binds the old code to the catalog");
}

#[tokio::test]
#[serial]
async fn dropping_a_formula_its_reader_still_names_is_refused_when_the_catalog_holds_the_code() {
    let (db, app, token) = setup().await;
    let calc = headspace_set(&db, &app, &token).await;
    let (uatm, _) = stored(&db, "hs_uatm").await;

    // hs_um published a catalog parameter of that code, which outlives the formula.
    let (status, body) = save(
        &app,
        &token,
        &calc,
        &json!({ "formulas": [
            { "id": uatm, "code": "hs_uatm", "name": "HS uatm", "units": "uatm", "formula": "hs_um / 3",
              "ordinal": 1 },
        ]}),
    )
    .await;
    assert_eq!(
        status, 400,
        "the reader would bind the catalog series: {body}"
    );
    let message = body.to_string();
    assert!(
        message.contains("hs_uatm") && message.contains("hs_um,"),
        "the refusal names the reader and the name: {message}"
    );
    let (_, formula) = stored(&db, "hs_um").await;
    assert_eq!(
        formula, "Dissolved_O2 * 2",
        "nothing of the refused save is written"
    );
}

#[tokio::test]
#[serial]
async fn a_shared_step_another_calculation_reads_is_refused_a_rename() {
    let (db, app, token) = setup().await;
    let first = calculation(&db, "first_set").await;
    let second = calculation(&db, "second_set").await;

    let (status, body) = save(
        &app,
        &token,
        &first,
        &json!({ "formulas": [
            { "code": "guard", "name": "Guard", "units": "uM", "formula": "Dissolved_O2 * 2",
              "ordinal": 0, "intermediate": true },
            { "code": "first_out", "name": "First out", "units": "uM", "formula": "guard + 1", "ordinal": 1 },
        ]}),
    )
    .await;
    assert_eq!(status, 200, "the first set saves: {body}");
    let (step, _) = stored(&db, "guard").await;
    let (first_out, _) = stored(&db, "first_out").await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": second, "formula_id": step }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "second declares the step: {body}"
    );
    let (status, body) = save(
        &app,
        &token,
        &second,
        &json!({ "formulas": [
            { "code": "second_out", "name": "Second out", "units": "uM", "formula": "guard * 3", "ordinal": 1 },
        ]}),
    )
    .await;
    assert_eq!(status, 200, "the second set saves: {body}");

    let (status, body) = save(
        &app,
        &token,
        &first,
        &json!({
            "formulas": [
                { "id": first_out, "code": "first_out", "name": "First out", "units": "uM",
                  "formula": "guard + 1", "ordinal": 1 },
            ],
            "shared_steps": [
                { "id": step, "code": "guard2", "name": "Guard", "units": "uM", "formula": "Dissolved_O2 * 2" },
            ],
        }),
    )
    .await;
    assert_eq!(status, 400, "second_set reads the step by its code: {body}");
    assert!(
        body.to_string().contains("second_set"),
        "the refusal names the reader: {body}"
    );
}
