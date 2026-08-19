//! The tool seed inserts only what is absent, so a re-run leaves the catalog as it found it.
//!
//! A seed that ran twice would otherwise duplicate a script name, a version 1 or an activation,
//! and the second copy would be the one an activation audit points at.
//!
//! It also seeds the same content identity the portal computes when an author saves a version, so
//! a provenance blob pinning a content hash pins the whole bundle whichever path created it.

use river_db::routes::private::tools::scripts::version_content_hash;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use sea_orm_migration::{MigratorTrait, SchemaManager};
use serial_test::serial;

use crate::support::{count, fresh_database, scalar};

const SEED: &str = "m20260818_000005_seed_tool_scripts";

/// Apply one named migration's `up` again against a fully migrated database.
async fn reapply(db: &DatabaseConnection, name: &str) {
    let manager = SchemaManager::new(db);
    let migrations = migration::Migrator::migrations();
    let migration = migrations
        .iter()
        .find(|m| m.name() == name)
        .unwrap_or_else(|| panic!("no migration named {name}"));
    migration
        .up(&manager)
        .await
        .unwrap_or_else(|e| panic!("re-applying {name} failed: {e}"));
}

/// Names that appear more than once, or scripts carrying more than one version or activation.
async fn duplicates(db: &DatabaseConnection) -> Vec<String> {
    let mut found = Vec::new();
    for (label, sql) in [
        (
            "script name",
            "SELECT LOWER(name) AS k, count(*) AS n FROM tool_scripts GROUP BY 1 HAVING count(*) > 1",
        ),
        (
            "version",
            "SELECT s.name AS k, count(*) AS n FROM tool_script_versions v \
             JOIN tool_scripts s ON s.id = v.tool_script_id GROUP BY 1 HAVING count(*) > 1",
        ),
        (
            "activation",
            "SELECT s.name AS k, count(*) AS n FROM tool_script_activations a \
             JOIN tool_scripts s ON s.id = a.tool_script_id GROUP BY 1 HAVING count(*) > 1",
        ),
    ] {
        for row in db
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                sql.to_string(),
            ))
            .await
            .expect("read the duplicate report")
        {
            let key: String = row.try_get("", "k").expect("k");
            let n: i64 = row.try_get("", "n").expect("n");
            found.push(format!("{label} {key}: {n}"));
        }
    }
    found
}

/// Every seeded hash, keyed by tool name.
async fn seeded_hashes(db: &DatabaseConnection) -> Vec<(String, String)> {
    db.query_all(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT s.name, v.content_hash FROM tool_script_versions v \
         JOIN tool_scripts s ON s.id = v.tool_script_id ORDER BY s.name"
            .to_string(),
    ))
    .await
    .expect("read the seeded hashes")
    .iter()
    .map(|row| {
        (
            row.try_get("", "name").expect("name"),
            row.try_get("", "content_hash").expect("content_hash"),
        )
    })
    .collect()
}

/// Scenario: the shipped tools are installed by a migration rather than posted to the authoring
/// endpoint.
///
/// Expected behaviour: every seeded hash recomputes from the row it sits on, through the same
/// helper the authoring path calls. `jsonb` is a parsed value rather than the text it arrived as,
/// so a hash taken over the seed files instead of over what was stored would identify bytes
/// nobody can read back: the alkalinity tolerance is written `1e-9` and reads back
/// `0.000000001`.
#[tokio::test]
#[serial]
async fn every_seeded_hash_recomputes_from_the_row_it_identifies() {
    let db = fresh_database("river_test_tool_seed_hash").await;
    migration::Migrator::up(&db, None)
        .await
        .expect("migrations apply");

    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.name, v.script, v.entry_function, v.manifest, v.test_cases, v.content_hash \
             FROM tool_script_versions v JOIN tool_scripts s ON s.id = v.tool_script_id \
             ORDER BY s.name"
                .to_string(),
        ))
        .await
        .expect("read the seeded versions");
    assert_eq!(rows.len(), 13, "every shipped tool was seeded");

    for row in &rows {
        let name: String = row.try_get("", "name").expect("name");
        let manifest: serde_json::Value = row.try_get("", "manifest").expect("manifest");
        let test_cases: serde_json::Value = row.try_get("", "test_cases").expect("test_cases");
        let stored: String = row.try_get("", "content_hash").expect("content_hash");
        let script: String = row.try_get("", "script").expect("script");
        let entry: String = row.try_get("", "entry_function").expect("entry_function");

        assert_eq!(
            version_content_hash(&script, &entry, &manifest, &test_cases),
            stored,
            "{name}: the stored hash does not describe the stored row"
        );
        assert_ne!(
            version_content_hash(
                &script,
                &entry,
                &serde_json::json!({ "label": "not the stored manifest" }),
                &test_cases
            ),
            stored,
            "{name}: the hash is blind to the manifest"
        );
    }

    let alkalinity_tolerance: Option<String> = scalar(
        &db,
        "SELECT v.test_cases->>'tolerance' AS v FROM tool_script_versions v \
         JOIN tool_scripts s ON s.id = v.tool_script_id WHERE s.name = 'alkalinity'",
    )
    .await;
    assert_eq!(
        alkalinity_tolerance.as_deref(),
        Some("0.000000001"),
        "the tolerance the seed file writes as 1e-9 is the one jsonb renormalises"
    );

    let hashes = seeded_hashes(&db).await;
    for (name, hash) in &hashes {
        assert!(
            hash.starts_with("sha256:"),
            "{name} carries {hash}, not a bundle hash"
        );
    }
    let mut distinct: Vec<&String> = hashes.iter().map(|(_, h)| h).collect();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), hashes.len(), "two tools share one hash");

    db.close().await.ok();
}

#[tokio::test]
#[serial]
async fn re_applying_the_tool_seed_changes_nothing() {
    let db = fresh_database("river_test_tool_seed_idempotency").await;
    migration::Migrator::up(&db, None)
        .await
        .expect("migrations apply");

    let scripts = count(&db, "SELECT count(*) AS v FROM tool_scripts").await;
    let versions = count(&db, "SELECT count(*) AS v FROM tool_script_versions").await;
    let activations = count(&db, "SELECT count(*) AS v FROM tool_script_activations").await;
    let hashes = seeded_hashes(&db).await;
    assert!(scripts > 0, "the seed installed something to re-seed");

    reapply(&db, SEED).await;

    assert_eq!(
        seeded_hashes(&db).await,
        hashes,
        "a second seed run moved a stored content hash"
    );

    assert_eq!(
        duplicates(&db).await,
        Vec::<String>::new(),
        "a second seed run duplicated rows"
    );
    assert_eq!(
        (
            count(&db, "SELECT count(*) AS v FROM tool_scripts").await,
            count(&db, "SELECT count(*) AS v FROM tool_script_versions").await,
            count(&db, "SELECT count(*) AS v FROM tool_script_activations").await,
        ),
        (scripts, versions, activations),
        "row counts moved across a re-run of the seed"
    );

    db.close().await.ok();
}
