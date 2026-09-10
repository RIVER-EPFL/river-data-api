//! Scenario: a database carried forward holds calculation formulas written before the entity
//! declared `name` and `units` required, so both columns are NULL on those rows.
//!
//! Expected behaviour: the migration fills them from what the row already carries and closes the
//! columns, so a whole-row entity read of the table stops failing on a missing value.

use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

use migration::m20260910_000034_formula_name_units_not_null::{DOWN, UP};
use river_db::routes::private::parameters::derived::definition_model;

async fn nullable(db: &DatabaseConnection, column: &str) -> bool {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT is_nullable = 'YES' AS v FROM information_schema.columns \
             WHERE table_name = 'calculation_formulas' AND column_name = '{column}'"
        ),
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get("", "v")
    .expect("is_nullable")
}

#[tokio::test]
#[serial]
async fn an_unnamed_formula_is_filled_from_its_code_and_the_columns_close() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec_unprepared(&db, DOWN).await;
    assert!(nullable(&db, "name").await, "reverted to the open column");

    let id = Uuid::new_v4();
    crate::common::exec_unprepared(
        &db,
        &format!(
            "INSERT INTO calculation_formulas (id, code, formula) \
             VALUES ('{id}', 'UnnamedFormula', 'a + b')"
        ),
    )
    .await;

    crate::common::exec_unprepared(&db, UP).await;

    assert!(!nullable(&db, "name").await);
    assert!(!nullable(&db, "units").await);

    let row = definition_model::Entity::find_by_id(id)
        .one(&db)
        .await
        .expect("a whole-row read of a formula that carried no name")
        .expect("the row");
    assert_eq!(row.name, "UnnamedFormula");
    assert_eq!(row.units, "");

    crate::common::cleanup_test_db(&db).await;
}
