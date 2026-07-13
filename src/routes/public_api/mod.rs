use crate::routes::public::views as handlers;

use axum::{Json, Router, extract::State, routing::get};
use utoipa::OpenApi;

use crate::common::AppState;
use crate::error::AppResult;
use crate::routes::public::service::list_public_codes;

// OpenAPI doc template for public APIs
#[derive(OpenApi)]
#[openapi(
    paths(
        handlers::list_sites,
        handlers::get_site,
        handlers::list_parameters,
        handlers::get_readings,
        handlers::get_aggregates,
    ),
    components(schemas(
        handlers::SiteRef,
        handlers::ParameterInfo,
        handlers::SiteDetailResponse,
        handlers::ReadingsResponse,
        handlers::ParameterData,
        handlers::AggregatesResponse,
        handlers::ParameterAggregateData,
    )),
    info(
        title = "Public Sensor Data API",
        description = "Environmental sensor time-series data.",
        version = "1.0.0"
    )
)]
pub struct PublicApiDoc;

/// Router for all public API integrations.
/// Routes: /`api/public/{project_code}/sites`/...
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/", get(discovery))
        .route("/{project_code}/sites", get(handlers::list_sites))
        .route("/{project_code}/sites/{site_code}", get(handlers::get_site))
        .route(
            "/{project_code}/sites/{site_code}/parameters",
            get(handlers::list_parameters),
        )
        .route(
            "/{project_code}/sites/{site_code}/readings",
            get(handlers::get_readings),
        )
        .route(
            "/{project_code}/sites/{site_code}/aggregates/{resolution}",
            get(handlers::get_aggregates),
        )
        .route("/{project_code}/docs", get(serve_docs))
}

#[derive(serde::Serialize)]
struct DiscoveryEntry {
    code: String,
    docs_url: String,
    sites_url: String,
}

async fn discovery(State(state): State<AppState>) -> AppResult<Json<Vec<DiscoveryEntry>>> {
    let codes = list_public_codes(&state.db).await?;
    let entries = codes
        .into_iter()
        .map(|code| DiscoveryEntry {
            docs_url: format!("/api/public/{code}/docs"),
            sites_url: format!("/api/public/{code}/sites"),
            code,
        })
        .collect();
    Ok(Json(entries))
}

async fn serve_docs(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(project_code): axum::extract::Path<String>,
) -> Result<axum::response::Html<String>, crate::error::AppError> {
    use crate::routes::public::service::get_public_config;

    let config = get_public_config(&state.db, &state.public_config_cache, &project_code).await?;

    let mut spec = PublicApiDoc::openapi();
    spec.info.title = config.api_title.clone();
    spec.info.description = Some(config.api_description.clone());
    spec.info.version = config.api_version.clone();

    // Add contact email if configured
    if let Some(email) = &config.contact_email {
        let mut contact = utoipa::openapi::info::Contact::new();
        contact.email = Some(email.clone());
        spec.info.contact = Some(contact);
    }

    // Set server URL so Scalar "Try It" points to the correct base path
    spec.servers = Some(vec![utoipa::openapi::server::Server::new(format!(
        "/api/public/{project_code}"
    ))]);

    // Rewrite paths: strip the /api/public/{project_code} prefix since server URL handles it
    let prefix = "/api/public/{project_code}";
    let old_paths: std::collections::BTreeMap<String, _> = std::mem::take(&mut spec.paths.paths);
    for (path_key, path_item) in old_paths {
        let new_key = if path_key.starts_with(prefix) {
            path_key
                .strip_prefix(prefix)
                .unwrap_or(&path_key)
                .to_string()
        } else {
            path_key
        };
        let new_key = if new_key.is_empty() {
            "/".to_string()
        } else {
            new_key
        };
        spec.paths.paths.insert(new_key, path_item);
    }

    let spec_json = serde_json::to_string(&spec).unwrap_or_default();

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <title>{title} - API Documentation</title>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
</head>
<body>
    <script id="api-reference" type="application/json">{spec_json}</script>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference@1.57.2"></script>
</body>
</html>"#,
        title = html_escape(&config.api_title),
        spec_json = spec_json,
    );

    Ok(axum::response::Html(html))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
