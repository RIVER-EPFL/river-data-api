//! Scenario: derived values are stored, and the formula that produced them is a mutable column.
//!
//! Expected behaviour: the migration versions each definition's current text as version 1 and
//! leaves the readings that predate it pointing at nothing, because what those rows were computed
//! with is not recoverable and naming today's formula would be a confident wrong answer (Q89,
//! M134). Two definitions writing one output parameter are refused by name rather than resolved.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use migration::m20260910_000014_derived_definition_versions::{DOWN, UP, formula_hash};
use migration::m20260910_000017_rename_calculation_formulas::{
    DOWN as RENAME_DOWN, UP as RENAME_UP,
};

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get_by_index::<i64>(0)
    .expect("a count")
}

/// A definition writing a catalog parameter of its own, and one stored derived reading of it.
async fn seed_definition(db: &DatabaseConnection, table: &str, code: &str, formula: &str) {
    crate::common::exec_unprepared(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES (gen_random_uuid(), '{code}', '{code}', 'mg/L', 'measurement'); \
             INSERT INTO {table} (id, code, name, units, formula, output_parameter_id) \
             SELECT gen_random_uuid(), '{code}_def', '{code}', 'mg/L', '{formula}', id \
               FROM parameters WHERE code = '{code}'"
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn the_current_text_becomes_version_one_and_stored_readings_name_none() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    // Migration 14 names the table as it was named then, so its replay runs before the rename.
    crate::common::exec_unprepared(&db, RENAME_DOWN).await;
    crate::common::exec_unprepared(&db, DOWN).await;

    seed_definition(&db, "derived_parameter_definitions", "DerivedVersioned", "a + b").await;
    crate::common::exec_unprepared(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index, measurement_type) \
             SELECT s.id, '{site}', p.id, '2025-03-01T00:00:00Z', 1.5, 0, 'derived' \
               FROM parameters p, data_streams s \
              WHERE p.code = 'DerivedVersioned' LIMIT 1",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;

    crate::common::exec_unprepared(&db, UP).await;
    mint_version_one(&db).await;

    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM derived_parameter_definition_versions v \
               JOIN derived_parameter_definitions d ON d.id = v.definition_id \
              WHERE d.code = 'DerivedVersioned_def' AND v.version_no = 1 AND v.formula = 'a + b'"
        )
        .await,
        1,
        "the definition's current text is version 1"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM readings r JOIN parameters p ON p.id = r.parameter_id \
              WHERE p.code = 'DerivedVersioned' AND r.derived_version_id IS NOT NULL"
        )
        .await,
        0,
        "a reading stored before versioning names no version rather than today's formula"
    );

    // Leave the schema as the rest of the suite reads it.
    crate::common::exec_unprepared(&db, RENAME_UP).await;
}

/// Expected behaviour: the resolver's single answer is already guaranteed, by the unique index
/// `m20260910_000007` added. Q89's decision named it, and it is not added twice.
#[tokio::test]
#[serial]
async fn one_definition_per_output_parameter_is_already_enforced() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    seed_definition(&db, "calculation_formulas", "DerivedClash", "a + b").await;
    let refused = db
        .execute_unprepared(
            "INSERT INTO calculation_formulas (id, code, name, units, formula, output_parameter_id) \
             SELECT gen_random_uuid(), 'DerivedClash_other', 'other', 'mg/L', 'a * b', output_parameter_id \
               FROM calculation_formulas WHERE code = 'DerivedClash_def'",
        )
        .await;
    assert!(
        refused.is_err(),
        "a second definition writing one output parameter is refused by the schema"
    );
}

/// The version-1 mint the migration does in Rust, so the stored hash is the one the runtime
/// helper computes for the same text.
async fn mint_version_one(db: &DatabaseConnection) {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.id::text AS id, d.formula FROM derived_parameter_definitions d \
              WHERE NOT EXISTS (SELECT 1 FROM derived_parameter_definition_versions v \
                                 WHERE v.definition_id = d.id)"
                .to_string(),
        ))
        .await
        .expect("definitions");
    for row in &rows {
        let id: String = row.try_get("", "id").expect("id");
        let formula: String = row.try_get("", "formula").expect("formula");
        crate::common::exec_unprepared(
            db,
            &format!(
                "INSERT INTO derived_parameter_definition_versions \
                     (definition_id, version_no, formula, content_hash, created_by) \
                 VALUES ('{id}'::uuid, 1, '{formula}', '{hash}', 'migration')",
                hash = formula_hash(&formula),
            ),
        )
        .await;
    }
}
