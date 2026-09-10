//! Every path the private OpenAPI document advertises is a URL the router resolves. A
//! `#[utoipa::path]` declares its path by hand and nothing ties it to where the handler is
//! mounted, so this is the only check that the served `/docs` describes reachable routes.
//!
//! Run: cargo test --test smoke openapi_paths -- --test-threads=1

use axum::body::Body;
use http_body_util::BodyExt;
use serial_test::serial;
use tower::ServiceExt;

const ID: &str = "00000000-0000-4000-8000-000000000001";

/// The site and project templates take seeded rows, because a site handler answers an unknown
/// site with a bare 404 that is indistinguishable from an unmatched route. Every other template
/// takes an id nothing owns; a handler's not-found for those carries a body.
fn concrete(path: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let end = rest[start..].find('}').expect("closed template") + start;
        let name = &rest[start + 1..end];
        let value = match name {
            "site_id" => crate::common::SITE1_ID,
            "id" if out.ends_with("/sites/") => crate::common::SITE1_ID,
            "project_id" => crate::common::PROJECT_ID,
            _ => ID,
        };
        out.push_str(value);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn operations(item: &utoipa::openapi::PathItem) -> Vec<&'static str> {
    let mut ops = Vec::new();
    if item.get.is_some() {
        ops.push("GET");
    }
    if item.post.is_some() {
        ops.push("POST");
    }
    if item.put.is_some() {
        ops.push("PUT");
    }
    if item.patch.is_some() {
        ops.push("PATCH");
    }
    if item.delete.is_some() {
        ops.push("DELETE");
    }
    ops
}

#[tokio::test]
#[serial]
async fn every_documented_path_resolves() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    // The user-management routes mount only when a Keycloak admin client is configured; the
    // credentials are never used because an API token is refused before any proxy call.
    let mut config = crate::common::test_config();
    config.keycloak_admin_client_id = Some("river-data-admin".into());
    config.keycloak_admin_client_secret = Some("unused".into());
    let state = river_db::common::AppState::new(db.clone(), config, None);
    let app = river_db::routes::build_router(state.clone());

    let spec = river_db::routes::openapi_spec(&state);
    assert!(
        spec.servers.is_none(),
        "paths are absolute, not relative to a server entry"
    );
    let mut unresolved = Vec::new();
    let mut checked = 0;
    for (path, item) in &spec.paths.paths {
        for method in operations(item) {
            checked += 1;
            let req = axum::http::Request::builder()
                .method(method)
                .uri(concrete(path))
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let fut = async {
                let response = app.clone().oneshot(req).await.unwrap();
                let status = response.status().as_u16();
                let body = response.into_body().collect().await.unwrap().to_bytes();
                (status, body.is_empty())
            };
            // axum's unmatched-route fallback is an empty 404; a handler's 404 carries a body,
            // and a response that never ends (the SSE stream) is a route that resolved.
            if let Ok((404, true)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), fut).await
            {
                unresolved.push(format!("{method} {path}"));
            }
        }
    }
    assert!(checked > 100, "the spec is populated: {checked} operations");
    assert!(
        unresolved.is_empty(),
        "{} of {checked} documented operations do not resolve:\n{}",
        unresolved.len(),
        unresolved.join("\n")
    );
}

/// Expected behaviour: the document carries the entity half the CrudCrate derive generates, not
/// only the hand-declared paths. The derive mounts eight routes per entity and builds a document
/// for them; the served spec is that document merged into the hand-written one.
#[tokio::test]
#[serial]
async fn the_spec_carries_the_generated_entity_routes() {
    let db = crate::common::setup_test_db().await;
    let state = river_db::common::AppState::new(db.clone(), crate::common::test_config(), None);
    let spec = river_db::routes::openapi_spec(&state);

    assert!(
        spec.paths.paths.contains_key("/api/site_parameters/{id}"),
        "a generated entity path is documented"
    );
    assert!(
        spec.paths
            .paths
            .contains_key("/api/sites/{site_id}/readings"),
        "a hand-declared path is still documented"
    );
    let components = spec
        .components
        .as_ref()
        .expect("the document declares components");
    assert!(
        components.schemas.contains_key("SiteParameterCreate"),
        "a generated create model is documented"
    );
}

/// The other direction: every route the service router mounts is in the document. `every_documented_path_resolves`
/// only catches a `#[utoipa::path]` whose path drifted from where its handler is mounted; a
/// handler with no annotation at all is invisible to it, which is how ten reachable endpoints
/// came to be undescribed.
///
/// The exemption list is the same shape the permission matrix keeps: a route absent from the
/// document on purpose says why here.
#[test]
fn every_registered_route_is_documented() {
    let db = sea_orm::DatabaseConnection::default();
    let mut config = crate::common::test_config();
    config.keycloak_admin_client_id = Some("river-data-admin".into());
    config.keycloak_admin_client_secret = Some("unused".into());
    let state = river_db::common::AppState::new(db, config, None);
    let spec = river_db::routes::openapi_spec(&state);

    // Routes deliberately outside the document, with the reason.
    let exempt = [
        // The enrollment pair speaks the sync protocol, whose contract is river-data-core's wire
        // types rather than an integrator-facing document.
        "/api/sync/enroll",
        "/api/sync/heartbeat",
    ];

    let sources: [(&str, &str); 5] = [
        ("src/routes/service/mod.rs", "/api"),
        ("src/routes/private/sync/views.rs", "/api/sync"),
        ("src/routes/private/projects/views.rs", "/api/projects"),
        ("src/routes/private/sites/views.rs", "/api/sites"),
        ("src/routes/private/admin/users.rs", "/api/users"),
    ];

    let mut undocumented = Vec::new();
    for (file, prefix) in sources {
        let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {file}: {e}"));
        for path in route_literals(&text) {
            let full = if path == "/" {
                prefix.to_string()
            } else {
                format!("{prefix}{path}")
            };
            if exempt.contains(&full.as_str()) {
                continue;
            }
            let normalised = full.trim_end_matches('/');
            if !spec.paths.paths.contains_key(&full) && !spec.paths.paths.contains_key(normalised) {
                undocumented.push(format!("{file}: {full}"));
            }
        }
    }
    assert!(
        undocumented.is_empty(),
        "{} routes are mounted and in no OpenAPI document:\n  {}",
        undocumented.len(),
        undocumented.join("\n  ")
    );
}

fn route_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route(".len()..];
        let Some(open) = rest.find('"') else { break };
        if rest[..open].chars().any(|c| !c.is_whitespace()) {
            continue;
        }
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        out.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
    out
}
