//! A parameter a calculation minted and no longer publishes: the catalogue says which calculation
//! left it behind, read from the formula rather than stored on the row.

use serde_json::json;
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn parameter_named(app: &axum::Router, token: &str, code: &str) -> serde_json::Value {
    let (status, body) =
        crate::common::get_json_with_token(app, "/api/parameters?perPage=500", token).await;
    assert_eq!(status, 200, "{body}");
    body.as_array()
        .expect("a list of parameters")
        .iter()
        .find(|p| p["code"] == code)
        .cloned()
        .unwrap_or_else(|| panic!("the catalogue holds {code}: {body}"))
}

#[tokio::test]
#[serial]
async fn a_formula_ticked_as_a_step_leaves_its_parameter_named_in_the_catalogue() {
    let (db, app, token) = setup().await;
    let code = "k1_step";

    let calculation =
        crate::common::seed_labelled_formula_calculation(&db, "k1_step_set", "Reaeration").await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": code,
            "name": "Rate constant",
            "units": "1/s",
            "formula": "Turbidity * 2",
            "tool_script_id": calculation,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{status}: {body}");
    let created: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    let formula_id = created["id"]
        .as_str()
        .expect("the formula's id")
        .to_string();

    let published = parameter_named(&app, &token, code).await;
    assert_eq!(
        published["unpublished_by"],
        serde_json::Value::Null,
        "a published output is an ordinary catalog row: {published}"
    );

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/derived_parameters/{formula_id}"),
        &json!({ "intermediate": true }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let given_up = parameter_named(&app, &token, code).await;
    assert_eq!(
        given_up["unpublished_by"], "Reaeration",
        "the catalogue names the calculation that gave the row up, not the formula: {given_up}"
    );

    let id = given_up["id"].as_str().expect("the parameter's id");
    let (status, one) =
        crate::common::get_json_with_token(&app, &format!("/api/parameters/{id}"), &token).await;
    assert_eq!(status, 200, "{one}");
    assert_eq!(
        one["unpublished_by"], "Reaeration",
        "the detail reads it the same way: {one}"
    );
}
