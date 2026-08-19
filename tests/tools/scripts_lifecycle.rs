//! Tool script authoring: versions are immutable and append-only, the lint blocks forbidden
//! constructs, activation requires validated cases, rollback is activating an older version,
//! and only a Keycloak Administrator can reach any of it.

use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;

#[tokio::test]
#[serial]
async fn no_api_token_reaches_the_authoring_surface() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db);

    for (method, uri) in [("GET", "/api/tool_scripts"), ("POST", "/api/tool_scripts")] {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        let response = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
        let status = response.status().as_u16();
        assert!(
            status == 401 || status == 403,
            "{method} {uri}: a full-permission token is still not an Administrator ({status})"
        );
    }
}

#[tokio::test]
#[serial]
async fn a_version_lives_through_lint_validate_activate_and_rollback() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_version_lives_through_lint_validate_activate_and_rollback",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_lifecycle").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    // `tool_scripts` holds the seeded tools, so it outlives the truncation and a rerun would
    // otherwise meet its own leftovers.
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = 'double_up'",
        "DELETE FROM tool_scripts WHERE name = 'double_up'",
    ] {
        crate::common::exec(&db, sql).await;
    }
    let app = kc::build_test_app_with_keycloak(db).await;
    kc::ensure_realm_user("scriptadmin", "scriptadmin", &["riverdata-admin"]).await;
    let admin = kc::get_keycloak_jwt("scriptadmin", "scriptadmin").await;

    let (status, script) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tool_scripts",
        &json!({ "name": "double_up", "label": "Double" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "script created: {script}");
    let sid = script["id"].as_str().unwrap().to_string();

    let manifest = json!({
        "label": "Double",
        "params": [ { "name": "x", "label": "X", "kind": "number", "required": true } ],
        "outputs": [ { "key": "doubled", "label": "Doubled", "per_replicate": false } ],
    });

    let (status, lint) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({
            "script": "tool <- function(inputs, constants, curves) { system('ls'); list() }",
            "manifest": manifest,
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "the lint refuses shell execution: {lint}");
    assert!(
        lint["detail"].to_string().contains("system"),
        "the finding names the construct and its line: {lint}"
    );

    let good = json!({
        "script": "tool <- function(inputs, constants, curves) list(doubled = 2 * inputs$x)",
        "manifest": manifest,
        "test_cases": { "tolerance": 1e-9, "cases": [
            { "name": "two", "inputs": { "x": 2 }, "expected": { "doubled": 4 } } ] },
    });
    let (status, v1) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &good,
        &admin,
    )
    .await;
    assert_eq!(status, 200, "version 1 created: {v1}");
    let v1_id = v1["version"]["id"].as_str().unwrap().to_string();

    let (status, dup) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &good,
        &admin,
    )
    .await;
    assert_eq!(status, 409, "an identical version is refused: {dup}");

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{v1_id}/activate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(
        status, 409,
        "activation before validation is refused: {refused}"
    );

    let (status, validated) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{v1_id}/validate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "validation ran: {validated}");
    assert_eq!(validated["passed"], true, "the case passes: {validated}");

    let (status, activated) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{v1_id}/activate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "activation lands: {activated}");

    let (status, calc) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/double_up/calculate",
        &json!({ "x": 21 }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "the new tool serves: {calc}");
    assert_eq!(calc["results"]["doubled"], 42, "{calc}");

    let v2_body = json!({
        "script": "tool <- function(inputs, constants, curves) list(doubled = 2 * inputs$x + 1)",
        "manifest": manifest,
        "test_cases": { "tolerance": 1e-9, "cases": [
            { "name": "two", "inputs": { "x": 2 }, "expected": { "doubled": 5 } } ] },
    });
    let (_, v2) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &v2_body,
        &admin,
    )
    .await;
    let v2_id = v2["version"]["id"].as_str().unwrap().to_string();
    for step in ["validate", "activate"] {
        let (status, body) = crate::common::post_json_parse_with_token(
            &app,
            &format!("/api/tool_scripts/{sid}/versions/{v2_id}/{step}"),
            &json!({}),
            &admin,
        )
        .await;
        assert_eq!(status, 200, "{step} on v2: {body}");
    }
    let (_, calc) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/double_up/calculate",
        &json!({ "x": 21 }),
        &admin,
    )
    .await;
    assert_eq!(calc["results"]["doubled"], 43, "v2 serves: {calc}");

    // Rollback: activating the older version, no new version required.
    let (status, rolled) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{v1_id}/activate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "rollback is an activation: {rolled}");
    let (_, calc) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/double_up/calculate",
        &json!({ "x": 21 }),
        &admin,
    )
    .await;
    assert_eq!(calc["results"]["doubled"], 42, "v1 serves again: {calc}");

    let (status, audit) = crate::common::get_json_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/activations"),
        &admin,
    )
    .await;
    assert_eq!(status, 200);
    let audit = audit.as_array().unwrap();
    assert_eq!(audit.len(), 3, "every flip is recorded: {audit:?}");
    assert_eq!(audit[0]["to_version_no"], 1, "newest first, the rollback");
    assert_eq!(audit[0]["from_version_no"], 2);
}
