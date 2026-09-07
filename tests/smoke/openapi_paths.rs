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
        spec.paths.paths.contains_key("/api/sites/{site_id}/readings"),
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
