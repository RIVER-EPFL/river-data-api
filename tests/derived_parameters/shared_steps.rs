//! A step two calculations compute is authored once and declared by each (Q156), and the step
//! itself answers which calculations read it.
//!
//! Run with: cargo test --test derived_parameters shared_steps

use serde_json::{Value, json};
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn put_json_with_token(
    app: &axum::Router,
    uri: &str,
    body: &Value,
    token: &str,
) -> (u16, String) {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let req = axum::http::Request::builder()
        .method("PUT")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a request");
    let response = app.clone().oneshot(req).await.expect("a response");
    let status = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn post(app: &axum::Router, uri: &str, body: &Value, token: &str) -> Value {
    let (status, parsed) = crate::common::post_json_parse_with_token(app, uri, body, token).await;
    assert!(
        (200..300).contains(&status),
        "POST {uri} refused with {status}: {parsed}"
    );
    parsed
}

fn id_of(created: &Value) -> String {
    created["id"].as_str().expect("an id").to_string()
}

/// A formula calculation, inserted directly: authoring one is an administrator's act behind a
/// Keycloak gate (`/api/tool_scripts`), and what is under test here is the step, not that gate.
async fn calculation(db: &sea_orm::DatabaseConnection, name: &str) -> String {
    let id = uuid::Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO tool_scripts (id, name, label, engine, enabled) \
             VALUES ('{id}', '{name}', '{name}', 'formula', true)"
        ),
    )
    .await;
    id.to_string()
}

/// A step of `first`, then the same step read by `second`: `guard` reads a seeded catalog
/// parameter, and each calculation has a formula of its own that names the step.
#[tokio::test]
#[serial]
async fn a_step_two_calculations_compute_is_authored_once_and_declared_by_each() {
    let (db, app, token) = setup().await;

    let first = calculation(&db, "first_set").await;
    let second = calculation(&db, "second_set").await;

    let step = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "guard", "name": "Guard", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": first,
            "ordinal": 0, "intermediate": true,
        }),
        &token,
    )
    .await;
    let step_id = id_of(&step);

    post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "first_out", "name": "First out", "units": "uM",
            "formula": "guard + 1", "tool_script_id": first, "ordinal": 1,
        }),
        &token,
    )
    .await;

    // The second calculation cannot write the step again: one code is one formula.
    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "guard", "name": "Guard", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": second,
            "ordinal": 0, "intermediate": true,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 409, "one code is one formula: {refused}");

    // It declares that it reads the step instead, which takes the step out of the first
    // calculation's ownership and leaves the first reading it the same way.
    post(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": second, "formula_id": step_id }),
        &token,
    )
    .await;

    // With the step declared, a formula of the second calculation resolves its code.
    post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "second_out", "name": "Second out", "units": "uM",
            "formula": "guard * 3", "tool_script_id": second, "ordinal": 1,
        }),
        &token,
    )
    .await;

    let (status, dependents) = crate::common::get_json_with_token(
        &app,
        &format!("/api/derived_parameters/{step_id}/dependents"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "({status}): {dependents}");
    assert_eq!(dependents["code"], "guard");
    assert_eq!(
        dependents["shared"], true,
        "the step belongs to no calculation: {dependents}"
    );
    let calculations = dependents["calculations"].as_array().expect("calculations");
    assert_eq!(
        calculations.len(),
        2,
        "both calculations read it: {dependents}"
    );
    let names: Vec<&str> = calculations
        .iter()
        .map(|c| c["name"].as_str().expect("a name"))
        .collect();
    assert!(
        names.contains(&"first_set") && names.contains(&"second_set"),
        "{dependents}"
    );
    for calculation in calculations {
        let readers: Vec<&str> = calculation["formulas"]
            .as_array()
            .expect("formulas")
            .iter()
            .map(|f| f["code"].as_str().expect("a code"))
            .collect();
        assert_eq!(
            readers.len(),
            1,
            "one formula of each names the step: {calculation}"
        );
        assert!(
            readers[0].ends_with("_out"),
            "the reader is the output, not the step itself: {calculation}"
        );
        assert_eq!(
            calculation["owns"], false,
            "a shared step is owned by nobody: {calculation}"
        );
    }
}

#[tokio::test]
#[serial]
async fn a_calculation_cannot_declare_a_step_it_already_computes() {
    let (db, app, token) = setup().await;
    let only = calculation(&db, "only_set").await;
    let step = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "own_step", "name": "Own step", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": only,
            "ordinal": 0, "intermediate": true,
        }),
        &token,
    )
    .await;

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": only, "formula_id": id_of(&step) }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "({status}): {refused}");
    assert!(
        refused.to_string().contains("already computes"),
        "{refused}"
    );
}

#[tokio::test]
#[serial]
async fn an_output_is_not_a_step_and_cannot_be_declared() {
    let (db, app, token) = setup().await;
    let first = calculation(&db, "output_set").await;
    let second = calculation(&db, "reader_set").await;
    let output = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "an_output", "name": "An output", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": first, "ordinal": 0,
        }),
        &token,
    )
    .await;

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        "/api/calculation_shared_steps",
        &json!({ "tool_script_id": second, "formula_id": id_of(&output) }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "({status}): {refused}");
    assert!(refused.to_string().contains("an output"), "{refused}");
}

/// A formula that was an output and is then ticked as a step gives up the parameter it published:
/// nothing stores a step's value, so a link left standing would keep the gap scan computing it
/// and keep the group calling the catalog parameter an Output.
#[tokio::test]
#[serial]
async fn a_formula_turned_into_a_step_gives_up_its_output_parameter() {
    let (db, app, token) = setup().await;
    let set = calculation(&db, "flip_set").await;

    let formula = post(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "was_an_output", "name": "Was an output", "units": "uM",
            "formula": "Dissolved_O2 * 2", "tool_script_id": set, "ordinal": 0,
        }),
        &token,
    )
    .await;
    assert!(
        formula["output_parameter_id"].is_string(),
        "an output publishes a catalog parameter: {formula}"
    );

    let (status, raw) = put_json_with_token(
        &app,
        &format!("/api/derived_parameters/{}", id_of(&formula)),
        &json!({ "intermediate": true }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "({status}): {raw}");
    let flipped: Value = serde_json::from_str(&raw).expect("JSON");
    assert!(
        flipped["output_parameter_id"].is_null(),
        "the response gives up the link: {flipped}"
    );

    let stored = crate::common::e2e::scalar(
        &db,
        &format!(
            "SELECT COALESCE(output_parameter_id::text, 'none') \
               FROM calculation_formulas WHERE id = '{}'",
            id_of(&formula)
        ),
    )
    .await;
    assert_eq!(stored, "none", "and so does the stored row");
}
