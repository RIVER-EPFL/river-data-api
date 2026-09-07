//! Scenario: a migration edits a seeded tool version's manifest in place.
//!
//! Expected behaviour: `content_hash` still identifies what the row holds. The hash is taken over
//! the stored jsonb, so an edit that leaves it behind makes the duplicate-version check and every
//! provenance blob pinning it point at content nobody can read back.

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;

use river_db::routes::private::tools::hash::stored_version_content;

#[tokio::test]
#[serial]
async fn every_seeded_version_hashes_to_what_it_stores() {
    let db = crate::common::setup_test_db().await;

    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.name, v.script, v.entry_function, v.manifest::text AS manifest, \
                    v.test_cases::text AS test_cases, v.content_hash \
               FROM tool_script_versions v JOIN tool_scripts s ON s.id = v.tool_script_id \
              WHERE s.created_by = 'seed'"
                .to_string(),
        ))
        .await
        .expect("seeded versions");
    assert!(!rows.is_empty(), "no seeded tool version to check");

    for row in rows {
        let name: String = row.try_get("", "name").expect("name");
        let script: String = row.try_get("", "script").expect("script");
        let entry: String = row.try_get("", "entry_function").expect("entry_function");
        let manifest: String = row.try_get("", "manifest").expect("manifest");
        let cases: String = row.try_get("", "test_cases").expect("test_cases");
        let stored: String = row.try_get("", "content_hash").expect("content_hash");

        let recomputed = stored_version_content(
            &db,
            &script,
            &entry,
            &serde_json::from_str(&manifest).expect("manifest is JSON"),
            &serde_json::from_str(&cases).expect("test cases are JSON"),
        )
        .await
        .expect("recompute")
        .content_hash;

        assert_eq!(
            stored, recomputed,
            "{name}: the stored hash is not the hash of the stored content"
        );
    }
}

#[tokio::test]
#[serial]
async fn the_doc_calculation_writes_the_portal_s_code() {
    let db = crate::common::setup_test_db().await;

    let code: String = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT v.manifest #>> '{params,0,parameter_code}' AS code \
               FROM tool_script_versions v JOIN tool_scripts s ON s.id = v.tool_script_id \
              WHERE s.name = 'doc' AND s.created_by = 'seed'"
                .to_string(),
        ))
        .await
        .expect("query")
        .expect("the doc version")
        .try_get("", "code")
        .expect("parameter_code");
    assert_eq!(code, "DOC_ppb");
}
