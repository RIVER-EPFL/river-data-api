use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

pub async fn get(app: &Router, uri: &str) -> (u16, String) {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

pub async fn get_with_token(app: &Router, uri: &str, token: &str) -> (u16, String) {
    let req = axum::http::Request::builder()
        .method("GET")
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

/// GET returning the response headers alongside the body, for endpoints whose pagination
/// contract lives in `Content-Range` rather than in the payload.
pub async fn get_with_token_headers(
    app: &Router,
    uri: &str,
    token: &str,
) -> (u16, axum::http::HeaderMap, String) {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();

    (status, headers, String::from_utf8_lossy(&body).to_string())
}

pub async fn get_json(app: &Router, uri: &str) -> (u16, serde_json::Value) {
    let (status, body) = get(app, uri).await;
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("Failed to parse JSON from {uri}: {e}\nBody: {body}"));
    (status, json)
}

pub async fn get_json_with_token(app: &Router, uri: &str, token: &str) -> (u16, serde_json::Value) {
    let (status, body) = get_with_token(app, uri, token).await;
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("Failed to parse JSON from {uri}: {e}\nBody: {body}"));
    (status, json)
}

/// Unauthenticated POST, used to assert the public tier refuses writes.
pub async fn post_json(app: &Router, uri: &str, body: &serde_json::Value) -> (u16, String) {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(body).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

pub async fn post_json_with_token(
    app: &Router,
    uri: &str,
    body: &serde_json::Value,
    token: &str,
) -> (u16, String) {
    let req = axum::http::Request::builder()
        .method("POST")
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

pub async fn post_json_parse_with_token(
    app: &Router,
    uri: &str,
    body: &serde_json::Value,
    token: &str,
) -> (u16, serde_json::Value) {
    let (status, text) = post_json_with_token(app, uri, body, token).await;
    let json: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("Failed to parse JSON from {uri}: {e}\nBody: {text}"));
    (status, json)
}

pub async fn patch_json_with_token(
    app: &Router,
    uri: &str,
    body: &serde_json::Value,
    token: &str,
) -> (u16, String) {
    let req = axum::http::Request::builder()
        .method("PATCH")
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

pub async fn patch_json_parse_with_token(
    app: &Router,
    uri: &str,
    body: &serde_json::Value,
    token: &str,
) -> (u16, serde_json::Value) {
    let (status, text) = patch_json_with_token(app, uri, body, token).await;
    let json: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("Failed to parse JSON from {uri}: {e}\nBody: {text}"));
    (status, json)
}

/// PATCH a pairing plan, naming the version it currently carries. A test is the only writer, so
/// reading the version immediately before the write is the same thing a reviewer's client does
/// with the version its last read returned.
pub async fn patch_plan_with_token(
    app: &Router,
    plan_id: &str,
    body: &serde_json::Value,
    token: &str,
) -> (u16, String) {
    let (status, plan) =
        get_json_with_token(app, &format!("/api/sync/pairing-plans/{plan_id}"), token).await;
    assert_eq!(status, 200, "reading the plan's version: {plan}");
    let mut body = body.clone();
    body["expected_version"] = plan["version"].clone();
    patch_json_with_token(app, &format!("/api/sync/pairing-plans/{plan_id}"), &body, token).await
}

/// POST a pairing plan action (`apply`, `revert`, `supersede`), naming the version it carries.
pub async fn post_plan_action_with_token(
    app: &Router,
    plan_id: &str,
    action: &str,
    token: &str,
) -> (u16, String) {
    let (status, plan) =
        get_json_with_token(app, &format!("/api/sync/pairing-plans/{plan_id}"), token).await;
    assert_eq!(status, 200, "reading the plan's version: {plan}");
    post_json_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}/{action}"),
        &serde_json::json!({ "expected_version": plan["version"] }),
        token,
    )
    .await
}

/// The parsing form of `post_plan_action_with_token`.
pub async fn post_plan_action_parse_with_token(
    app: &Router,
    plan_id: &str,
    action: &str,
    token: &str,
) -> (u16, serde_json::Value) {
    let (status, text) = post_plan_action_with_token(app, plan_id, action, token).await;
    let json: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("Failed to parse JSON from plan {action}: {e}\nBody: {text}"));
    (status, json)
}

pub async fn put_json_with_token(
    app: &Router,
    uri: &str,
    body: &serde_json::Value,
    token: &str,
) -> (u16, String) {
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

pub async fn delete_json_with_token(
    app: &Router,
    uri: &str,
    body: &serde_json::Value,
    token: &str,
) -> (u16, String) {
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

pub async fn delete_with_token(app: &Router, uri: &str, token: &str) -> (u16, String) {
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

pub async fn get_with_auth_header(app: &Router, uri: &str, auth_value: &str) -> (u16, String) {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", auth_value)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

pub async fn get_csv_with_token(app: &Router, uri: &str, token: &str) -> (u16, String) {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "text/csv")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

pub async fn get_ndjson_with_token(app: &Router, uri: &str, token: &str) -> (u16, String) {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/x-ndjson")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

/// Import a CSV as a screened commit: preview it first and pass the seasonal check the preview
/// stored, the way the import page does. A spot or tool file whose values sit outside the site's
/// seasonal range is otherwise refused without a check. `body` must not set `dry_run`.
pub async fn post_screened_import(
    app: &Router,
    body: &serde_json::Value,
    token: &str,
) -> (u16, serde_json::Value) {
    let mut preview = body.clone();
    preview["dry_run"] = serde_json::json!(true);
    let (status, plan) =
        post_json_parse_with_token(app, "/api/readings/import_csv", &preview, token).await;
    if status != 200 {
        return (status, plan);
    }
    let mut commit = body.clone();
    if let Some(check_id) = plan["check"]["check_id"].as_str() {
        commit["check_id"] = serde_json::json!(check_id);
    }
    post_json_parse_with_token(app, "/api/readings/import_csv", &commit, token).await
}

/// Serve a router on a loopback port and return its base URL, for the tests that need a real
/// HTTP client rather than `oneshot`: a mock upstream the app calls out to, and the sync driver,
/// which reaches the API through `API_BASE_URL` and cannot be handed a `Router`.
///
/// The server lives as long as the test process; there is nothing to shut down, because a test
/// binary that has finished takes its listeners with it.
pub async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a free loopback port");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        axum::serve(listener, river_db::routes::connected_service(app))
            .await
            .expect("serve the router");
    });
    format!("http://{addr}")
}
