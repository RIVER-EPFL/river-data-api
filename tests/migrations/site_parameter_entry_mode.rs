//! Scenario: a database carried forward declares its computed slots with `is_derived` and names
//! the definition on the slot.
//!
//! Expected behaviour: the declaration survives the migration under the name it now means, and
//! the definition reference goes, because which calculation produces a parameter is the group's
//! binding and not the slot's.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use migration::m20260910_000007_site_parameter_entry_mode::{DOWN, UP};
use migration::m20260910_000017_rename_calculation_formulas::{
    DOWN as RENAME_DOWN, UP as RENAME_UP,
};

async fn entry_mode(db: &DatabaseConnection, name: &str) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT entry_mode FROM site_parameters WHERE name = '{name}'"),
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get("", "entry_mode")
    .expect("entry_mode")
}

/// A slot at site 1 holding a catalog parameter the seed does not already assign there.
async fn seed_slot(db: &DatabaseConnection, name: &str, code: &str, derived: bool) {
    crate::common::exec_unprepared(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES (gen_random_uuid(), '{code}', '{code}', 'mg/L', 'measurement'); \
             INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, is_derived) \
             SELECT gen_random_uuid(), '{site}', id, '{name}', 'derived', {derived} \
               FROM parameters WHERE code = '{code}'",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn the_declaration_survives_and_the_definition_reference_goes() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    // The migration names the formula table as it was named then, so its replay runs before the
    // rename.
    crate::common::exec_unprepared(&db, RENAME_DOWN).await;
    crate::common::exec_unprepared(&db, DOWN).await;

    seed_slot(&db, "computed here", "EntryModeComputed", true).await;
    seed_slot(&db, "typed by hand", "EntryModeManual", false).await;

    crate::common::exec_unprepared(&db, UP).await;

    assert_eq!(entry_mode(&db, "computed here").await, "tool");
    assert_eq!(entry_mode(&db, "typed by hand").await, "manual");

    let columns: i64 = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS c FROM information_schema.columns \
              WHERE table_name = 'site_parameters' \
                AND column_name IN ('is_derived', 'derived_definition_id')"
                .to_string(),
        ))
        .await
        .expect("query")
        .expect("a row")
        .try_get("", "c")
        .expect("count");
    assert_eq!(columns, 0, "both columns are gone");

    // Leave the schema as the rest of the suite reads it.
    crate::common::exec_unprepared(&db, RENAME_UP).await;
    crate::common::cleanup_test_db(&db).await;
}

/// An output is produced by exactly one calculation, which is what lets a slot resolve its
/// definition from its own parameter.
#[tokio::test]
#[serial]
async fn a_second_definition_cannot_claim_an_output() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    for code in ["one", "two"] {
        let result = db
            .execute_unprepared(&format!(
                "INSERT INTO calculation_formulas (id, code, name, formula, output_parameter_id) \
                 VALUES (gen_random_uuid(), '{code}', '{code}', 'a * 2', '{param}')",
                param = crate::common::GLOBAL_PARAM_DO_ID,
            ))
            .await;
        if code == "one" {
            result.expect("the first definition claims the output");
        } else {
            assert!(result.is_err(), "a second definition is refused the output");
        }
    }

    crate::common::cleanup_test_db(&db).await;
}
