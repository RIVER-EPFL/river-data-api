//! E2E tests for the derived formula pipeline.
//!
//! Run with: cargo test --test derived_formula_tests
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.

mod common;

use serial_test::serial;

// ============================================================================
// Helper: setup, cleanup, seed, and build app
// ============================================================================

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());
    (db, app, token)
}

// ============================================================================
// HTTP helpers for PUT and DELETE (not in common)
// ============================================================================

async fn put_json_with_token(
    app: &axum::Router,
    uri: &str,
    body: &serde_json::Value,
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
        .body(Body::from(serde_json::to_string(body).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

async fn delete_with_token(app: &axum::Router, uri: &str, token: &str) -> (u16, String) {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

/// Create a derived parameter and return its id (for cleanup).
async fn create_derived_param(
    app: &axum::Router,
    token: &str,
    name: &str,
    formula: &str,
) -> (u16, serde_json::Value) {
    let body = serde_json::json!({
        "name": name,
        "display_name": format!("Test {name}"),
        "units": "test_units",
        "formula": formula,
        "description": "Auto-created by test",
        "required_parameter_types": []
    });
    let (status, text) =
        common::post_json_with_token(app, "/api/service/derived_parameters", &body, token).await;
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|_| {
        serde_json::json!({ "raw": text })
    });
    (status, json)
}

/// Delete a derived parameter by id (best-effort cleanup).
async fn cleanup_derived_param(app: &axum::Router, token: &str, id: &str) {
    let uri = format!("/api/service/derived_parameters/{id}");
    let _ = delete_with_token(app, &uri, token).await;
}

// =============================================================================
// 1. Create with valid formula — required_parameter_types auto-populated
// =============================================================================

#[tokio::test]
#[serial]
async fn test_create_derived_parameter_valid_formula() {
    let (_db, app, token) = setup().await;
    let name = format!("valid_formula_{}", uuid::Uuid::new_v4());

    let (status, json) =
        create_derived_param(&app, &token, &name, "sqrt(Turbidity) * 2 + CDOM").await;

    assert!(
        (200..300).contains(&status),
        "Expected 2xx, got {status}: {json}"
    );

    let id = json["id"].as_str().expect("response should have id");
    assert_eq!(json["formula"], "sqrt(Turbidity) * 2 + CDOM");

    // required_parameter_types should be auto-populated with the variable names
    let rpt = json["required_parameter_types"]
        .as_array()
        .expect("required_parameter_types should be array");
    let rpt_strings: Vec<&str> = rpt.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        rpt_strings.contains(&"Turbidity"),
        "required_parameter_types should contain Turbidity, got: {rpt_strings:?}"
    );
    assert!(
        rpt_strings.contains(&"CDOM"),
        "required_parameter_types should contain CDOM, got: {rpt_strings:?}"
    );
    assert_eq!(
        rpt_strings.len(),
        2,
        "should have exactly 2 required parameter types, got: {rpt_strings:?}"
    );

    cleanup_derived_param(&app, &token, id).await;
}

// =============================================================================
// 2. Create with invalid formula — 400
// =============================================================================

#[tokio::test]
#[serial]
async fn test_create_derived_parameter_invalid_formula() {
    let (_db, app, token) = setup().await;
    let name = format!("invalid_formula_{}", uuid::Uuid::new_v4());

    let (status, _json) = create_derived_param(&app, &token, &name, "sqrt(").await;

    assert_eq!(status, 400, "Invalid formula should return 400");
}

// =============================================================================
// 3. Create with constants only — required_parameter_types is empty
// =============================================================================

#[tokio::test]
#[serial]
async fn test_create_derived_parameter_constants_only() {
    let (_db, app, token) = setup().await;
    let name = format!("constants_only_{}", uuid::Uuid::new_v4());

    let (status, json) = create_derived_param(&app, &token, &name, "pi * 2 + 1").await;

    assert!(
        (200..300).contains(&status),
        "Expected 2xx, got {status}: {json}"
    );

    let id = json["id"].as_str().expect("response should have id");

    let rpt = json["required_parameter_types"]
        .as_array()
        .expect("required_parameter_types should be array");
    assert!(
        rpt.is_empty(),
        "Constants-only formula should have empty required_parameter_types, got: {rpt:?}"
    );

    cleanup_derived_param(&app, &token, id).await;
}

// =============================================================================
// 4. Update formula — required_parameter_types changes
// =============================================================================

#[tokio::test]
#[serial]
async fn test_update_derived_parameter_formula() {
    let (_db, app, token) = setup().await;
    let name = format!("update_formula_{}", uuid::Uuid::new_v4());

    // Create with initial formula
    let (status, json) = create_derived_param(&app, &token, &name, "Turbidity * 2").await;
    assert!(
        (200..300).contains(&status),
        "Create should succeed: {json}"
    );
    let id = json["id"].as_str().expect("response should have id");

    // Verify initial required_parameter_types
    let rpt = json["required_parameter_types"]
        .as_array()
        .expect("required_parameter_types should be array");
    let rpt_strings: Vec<&str> = rpt.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        rpt_strings.contains(&"Turbidity"),
        "Initial rpt should contain Turbidity"
    );
    assert!(
        !rpt_strings.contains(&"Conductivity"),
        "Initial rpt should NOT contain Conductivity"
    );

    // Update formula to reference different variables
    let update_body = serde_json::json!({
        "formula": "Conductivity + Depth"
    });
    let uri = format!("/api/service/derived_parameters/{id}");
    let (put_status, put_text) = put_json_with_token(&app, &uri, &update_body, &token).await;
    let put_json: serde_json::Value = serde_json::from_str(&put_text).unwrap_or_else(|_| {
        serde_json::json!({ "raw": put_text })
    });

    assert!(
        (200..300).contains(&put_status),
        "Update should succeed, got {put_status}: {put_json}"
    );

    // Verify required_parameter_types changed
    let updated_rpt = put_json["required_parameter_types"]
        .as_array()
        .expect("updated required_parameter_types should be array");
    let updated_strings: Vec<&str> = updated_rpt.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        updated_strings.contains(&"Conductivity"),
        "Updated rpt should contain Conductivity, got: {updated_strings:?}"
    );
    assert!(
        updated_strings.contains(&"Depth"),
        "Updated rpt should contain Depth, got: {updated_strings:?}"
    );
    assert!(
        !updated_strings.contains(&"Turbidity"),
        "Updated rpt should NOT contain Turbidity anymore, got: {updated_strings:?}"
    );

    cleanup_derived_param(&app, &token, id).await;
}

// =============================================================================
// 5. Preview derived with nonexistent site — graceful error, not 500
// =============================================================================

#[tokio::test]
#[serial]
async fn test_preview_derived_missing_site() {
    let (_db, app, token) = setup().await;
    let fake_site_id = uuid::Uuid::new_v4();

    let body = serde_json::json!({
        "formula": "Turbidity * 2",
        "site_id": fake_site_id.to_string(),
        "start": "2025-01-15T00:00:00Z",
        "end": "2025-01-16T00:00:00Z"
    });

    let (status, _text) =
        common::post_json_with_token(&app, "/api/service/actions/preview_derived", &body, &token)
            .await;

    // Should be a client error (404) not a server error (500)
    assert_ne!(
        status, 500,
        "Missing site should not cause a 500 server error"
    );
    assert!(
        (400..500).contains(&status),
        "Expected 4xx for missing site, got {status}"
    );
}

// =============================================================================
// 6. Preview derived with invalid formula — 400
// =============================================================================

#[tokio::test]
#[serial]
async fn test_preview_derived_invalid_formula() {
    let (_db, app, token) = setup().await;

    let body = serde_json::json!({
        "formula": ")))",
        "site_id": common::SITE1_ID,
        "start": "2025-01-15T00:00:00Z",
        "end": "2025-01-16T00:00:00Z"
    });

    let (status, _text) =
        common::post_json_with_token(&app, "/api/service/actions/preview_derived", &body, &token)
            .await;

    assert_eq!(
        status, 400,
        "Invalid formula in preview should return 400"
    );
}

// =============================================================================
// 7. Formula injection attempts — meval rejects non-math expressions
// =============================================================================

#[tokio::test]
#[serial]
async fn test_formula_injection_attempt() {
    let (_db, app, token) = setup().await;

    let malicious_formulas = vec![
        "std::process::exit(1)",
        "import os",
        r#"eval("rm -rf /")"#,
        "system('ls')",
        "__import__('os').system('id')",
    ];

    for formula in malicious_formulas {
        let name = format!("injection_{}", uuid::Uuid::new_v4());
        let (status, json) = create_derived_param(&app, &token, &name, formula).await;

        // meval should either reject the formula as unparseable (400) or treat
        // the tokens as unknown variables (2xx with those tokens in required_parameter_types).
        // Either way, no code execution should occur.
        assert_ne!(
            status, 500,
            "Formula '{formula}' should not cause 500. Response: {json}"
        );

        // If it was accepted (2xx), clean up
        if (200..300).contains(&status) {
            if let Some(id) = json["id"].as_str() {
                cleanup_derived_param(&app, &token, id).await;
            }
        }
    }
}

// =============================================================================
// 8. Boundary: sqrt of negative — meval returns NaN, does not crash
// =============================================================================

#[test]
fn test_formula_boundary_sqrt_negative() {
    let expr: meval::Expr = "sqrt(x)".parse().unwrap();
    let func = expr.bind("x").unwrap();

    let result = func(-1.0);
    // meval should return NaN for sqrt of a negative number, not panic
    assert!(
        result.is_nan(),
        "sqrt(-1) should be NaN, got {result}"
    );

    // Positive case still works
    let positive = func(4.0);
    assert!(
        (positive - 2.0).abs() < 1e-10,
        "sqrt(4) should be 2.0, got {positive}"
    );
}

// =============================================================================
// 9. Boundary: division by zero — meval returns Inf, does not crash
// =============================================================================

#[test]
fn test_formula_boundary_division_by_zero() {
    let expr: meval::Expr = "x / y".parse().unwrap();
    let func = expr.bind2("x", "y").unwrap();

    let result = func(1.0, 0.0);
    // meval should return Inf for division by zero, not panic
    assert!(
        result.is_infinite(),
        "1/0 should be Inf, got {result}"
    );

    // 0/0 should be NaN
    let nan_result = func(0.0, 0.0);
    assert!(
        nan_result.is_nan(),
        "0/0 should be NaN, got {nan_result}"
    );

    // Normal division still works
    let normal = func(6.0, 3.0);
    assert!(
        (normal - 2.0).abs() < 1e-10,
        "6/3 should be 2.0, got {normal}"
    );
}
