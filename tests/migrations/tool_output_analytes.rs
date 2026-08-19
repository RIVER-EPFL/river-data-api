//! Every analyte a seeded tool output names exists in the catalog once the migrations have run.
//!
//! An output whose `suggested_parameter_code` matches no `parameters` row resolves to nothing, so
//! the save panel has nowhere to write the calculated value and the operator's only recourse is to
//! create the analyte by hand and guess its spelling. The check is driven from the manifests the
//! seed actually stored rather than from a list here, so a tool published later naming an analyte
//! nobody created fails at this test instead of at the save.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use serial_test::serial;

use crate::support::fresh_database;

/// `(tool name, output key, suggested code)` for every output of every active version that names a
/// code with no matching `parameters` row. Resolution is case-insensitive, matching
/// `ParameterCatalog::resolve` and the catalog's unique index on `LOWER(code)`.
async fn unresolved_codes(db: &DatabaseConnection) -> Vec<String> {
    db.query_all(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT s.name AS tool, o->>'key' AS key, o->>'suggested_parameter_code' AS code
         FROM tool_scripts s
         JOIN tool_script_versions v ON v.id = s.active_version_id
         CROSS JOIN LATERAL jsonb_array_elements(v.manifest->'outputs') AS o
         WHERE o->>'suggested_parameter_code' IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM parameters p
               WHERE LOWER(p.code) = LOWER(o->>'suggested_parameter_code'))
         ORDER BY 1, 2"
            .to_string(),
    ))
    .await
    .expect("read the unresolved-code report")
    .into_iter()
    .map(|row| {
        let tool: String = row.try_get("", "tool").expect("tool");
        let key: String = row.try_get("", "key").expect("key");
        let code: String = row.try_get("", "code").expect("code");
        format!("{tool}.{key} names {code}")
    })
    .collect()
}

async fn count_of(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .expect("count query")
    .expect("one row")
    .try_get::<i64>("", "v")
    .expect("column v")
}

#[tokio::test]
#[serial]
async fn every_seeded_tool_output_resolves_to_a_catalog_parameter() {
    let db = fresh_database("river_test_tool_output_analytes").await;
    migration::Migrator::up(&db, None)
        .await
        .expect("migrations apply");

    let coded = count_of(
        &db,
        "SELECT count(*) AS v
         FROM tool_scripts s
         JOIN tool_script_versions v ON v.id = s.active_version_id
         CROSS JOIN LATERAL jsonb_array_elements(v.manifest->'outputs') AS o
         WHERE o->>'suggested_parameter_code' IS NOT NULL",
    )
    .await;
    assert!(
        coded > 0,
        "the seed installed outputs carrying a code, otherwise this test asserts nothing"
    );

    assert_eq!(
        unresolved_codes(&db).await,
        Vec::<String>::new(),
        "a tool output names an analyte the catalog does not hold"
    );

    // The seed inserts its manifests directly, so `check_manifest_against_catalog` never sees
    // them. These are the two rules it would have applied, asserted where the seed can break them.
    assert_eq!(
        count_of(
            &db,
            "SELECT count(*) AS v
             FROM tool_script_versions v
             CROSS JOIN LATERAL jsonb_array_elements(v.manifest->'outputs') AS o
             WHERE o->>'parameter_id' IS NOT NULL",
        )
        .await,
        0,
        "a seed file names a parameter_id, which no other database holds"
    );

    assert_eq!(
        count_of(
            &db,
            "SELECT count(*) AS v FROM (
                 SELECT s.id, LOWER(o->>'suggested_parameter_code')
                 FROM tool_scripts s
                 JOIN tool_script_versions v ON v.id = s.active_version_id
                 CROSS JOIN LATERAL jsonb_array_elements(v.manifest->'outputs') AS o
                 WHERE o->>'suggested_parameter_code' IS NOT NULL
                 GROUP BY 1, 2 HAVING count(*) > 1
             ) AS collisions",
        )
        .await,
        0,
        "two outputs of one tool save to the same parameter"
    );

    db.close().await.ok();
}

/// Scenario: the migration is rolled back while one of the analytes it created is referenced by a
/// table other than `site_parameters`.
///
/// Expected behaviour: that row stays and the rollback completes. A dozen tables reference
/// `parameters` with NO ACTION, so a rollback that only knew about `site_parameters` would abort
/// the whole transaction on any of the others.
#[tokio::test]
#[serial]
async fn a_referenced_analyte_survives_the_rollback_and_the_rest_are_removed() {
    let db = fresh_database("river_test_tool_analyte_rollback").await;
    migration::Migrator::up(&db, None)
        .await
        .expect("migrations apply");

    crate::support::exec(
        &db,
        "INSERT INTO alarm_thresholds (parameter_id, alarm_max)
         SELECT id, 1 FROM parameters WHERE code = 'SUVA'",
    )
    .await;

    migration::Migrator::down(
        &db,
        Some(crate::support::steps_back_through(
            "m20260818_000008_tool_output_analytes",
        )),
    )
    .await
    .expect("the rollback skips the referenced row instead of aborting");

    assert_eq!(
        count_of(
            &db,
            "SELECT count(*) AS v FROM parameters WHERE code = 'SUVA'"
        )
        .await,
        1,
        "an analyte something still points at is left in place"
    );
    assert_eq!(
        count_of(
            &db,
            "SELECT count(*) AS v FROM parameters WHERE code = 'A_T'"
        )
        .await,
        0,
        "an analyte nothing references is removed"
    );

    db.close().await.ok();
}
