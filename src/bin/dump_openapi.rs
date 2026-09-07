//! Write the private API's OpenAPI document to a file, so the served spec is a committed artefact
//! rather than something only a running server can answer for.
//!
//! `cargo run --bin dump_openapi -- docs/openapi.json`, defaulting to that path. It builds the
//! router to collect the entity half of the document and never issues a query, so the connection
//! it carries is a disconnected one and no database is needed.

use std::path::PathBuf;

use river_db::common::AppState;
use river_db::config::Config;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "docs/openapi.json".to_string())
        .into();
    // The user-management routes mount only when a Keycloak admin client is configured, so the
    // dump declares one; the credentials are never used because nothing is served.
    let mut config = Config::from_env().map_err(|e| {
        format!("{e}. The dump builds the router and serves nothing, so any URL will do.")
    })?;
    config.keycloak_admin_client_id = Some("river-data-admin".into());
    config.keycloak_admin_client_secret = Some("unused".into());
    let state = AppState::new(sea_orm::DatabaseConnection::default(), config, None);
    let spec = river_db::routes::openapi_spec(&state);
    let json = river_db::routes::openapi_json(&spec)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, json)?;
    println!("{} paths written to {}", spec.paths.paths.len(), path.display());
    Ok(())
}
