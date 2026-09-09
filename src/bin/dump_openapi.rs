//! Write the private API's OpenAPI document to a file, so the served spec is a committed artefact
//! rather than something only a running server can answer for.
//!
//! `cargo run --bin dump_openapi -- docs/openapi.json`, defaulting to that path. It builds the
//! router to collect the entity half of the document and never issues a query, so it needs neither
//! a database nor any configuration.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "docs/openapi.json".to_string())
        .into();
    let json = river_db::routes::committed_document()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, &json)?;
    println!("{} bytes written to {}", json.len(), path.display());
    Ok(())
}
