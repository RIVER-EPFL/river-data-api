//! Scenario: a tool version stored before `m20260916_000001_sample_sd_only` carries an output
//! declaring `sd_estimator`, which `ManifestOutput` now refuses as an unknown field.
//!
//! Expected behaviour: the migration strips the key from every output, the manifest deserializes
//! afterwards with its outputs otherwise untouched, and the version keeps its `content_hash`.
//!
//! Run: cargo test --test migrations manifest_sd_estimator_strip -- --test-threads=1

use river_db::routes::private::tools::models::Manifest;
use sea_orm::{ConnectionTrait, Database, Statement};
use sea_orm_migration::MigratorTrait;
use serial_test::serial;

use crate::common::scratch;

const STRIP: &str = "m20260916_000001_sample_sd_only";

const MANIFEST: &str = r#"{
    "label": "DOC",
    "params": [{"kind": "replicates", "name": "DOC", "label": "DOC", "units": "ppb",
                "parameter_code": "DOC_ppb"}],
    "outputs": [
        {"key": "DOC_avg_ppb", "label": "DOC average", "units": "ppb",
         "aggregate": "mean", "aggregate_of": "DOC"},
        {"key": "DOC_sd_ppb", "label": "DOC standard deviation", "units": "ppb",
         "aggregate": "sd", "aggregate_of": "DOC", "sd_estimator": "population"}
    ]
}"#;

#[tokio::test]
#[serial]
async fn a_stored_output_declaring_sd_estimator_loads_after_the_strip() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let name = format!("river_sd_strip_{}", std::process::id());
    let server = scratch::server(&base).await;
    scratch::discard(&server, &name).await;
    server
        .execute_unprepared(&format!("CREATE DATABASE {name}"))
        .await
        .expect("create the scratch database");
    let db = Database::connect(scratch::url_for(&base, &name))
        .await
        .expect("connect to the scratch database");

    let before = migration::Migrator::migrations()
        .iter()
        .position(|m| m.name() == STRIP)
        .expect("the strip is registered");
    let before = u32::try_from(before).expect("a step count");
    migration::Migrator::up(&db, Some(before))
        .await
        .expect("migrate to the step before the strip");

    db.execute_unprepared(&format!(
        "INSERT INTO tool_scripts (id, name, label) \
         VALUES ('33333333-3333-3333-3333-333333333333', 'doc_strip', 'DOC'); \
         INSERT INTO tool_script_versions (tool_script_id, version_no, script, manifest, content_hash) \
         VALUES ('33333333-3333-3333-3333-333333333333', 1, 'tool <- function(inputs, constants, curves) list()', \
                 '{}'::jsonb, 'hash-before-strip')",
        MANIFEST.replace('\'', "''")
    ))
    .await
    .expect("seed a version declaring sd_estimator");

    migration::Migrator::up(&db, None)
        .await
        .expect("apply the remaining steps");

    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT manifest, content_hash FROM tool_script_versions \
             WHERE tool_script_id = '33333333-3333-3333-3333-333333333333'",
        ))
        .await
        .expect("read the version")
        .expect("the version survives");
    let stored: serde_json::Value = row.try_get("", "manifest").expect("manifest");
    let hash: String = row.try_get("", "content_hash").expect("content_hash");

    let outputs = stored["outputs"].as_array().expect("outputs stay an array");
    assert!(
        outputs.iter().all(|o| o.get("sd_estimator").is_none()),
        "no output keeps sd_estimator: {stored}"
    );
    let manifest: Manifest =
        serde_json::from_value(stored.clone()).expect("the stripped manifest deserializes");
    let shape: Vec<(&str, &str, Option<&str>, Option<&str>)> = manifest
        .outputs
        .iter()
        .map(|o| {
            (
                o.key.as_str(),
                o.label.as_str(),
                o.aggregate.as_deref(),
                o.aggregate_of.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("DOC_avg_ppb", "DOC average", Some("mean"), Some("DOC")),
            (
                "DOC_sd_ppb",
                "DOC standard deviation",
                Some("sd"),
                Some("DOC")
            ),
        ]
    );
    assert_eq!(
        hash, "hash-before-strip",
        "the version keeps its content_hash"
    );

    drop(db);
    scratch::discard(&server, &name).await;
}
