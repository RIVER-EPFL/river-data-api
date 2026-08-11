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

pub async fn get_json(app: &Router, uri: &str) -> (u16, serde_json::Value) {
    let (status, body) = get(app, uri).await;
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("Failed to parse JSON from {uri}: {e}\nBody: {body}"));
    (status, json)
}

pub async fn get_json_with_token(
    app: &Router,
    uri: &str,
    token: &str,
) -> (u16, serde_json::Value) {
    let (status, body) = get_with_token(app, uri, token).await;
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("Failed to parse JSON from {uri}: {e}\nBody: {body}"));
    (status, json)
}

/// Unauthenticated POST — used to assert the public tier refuses writes.
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
