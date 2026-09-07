//! Scenario: a database is reverted past the migration that made `schedule_audit` into
//! `change_audit`, having since gained the parameter and site-parameter trail
//! `m20260910_000015` writes.
//!
//! Expected behaviour: the revert restores the table the migration found, holding the schedule
//! trail and nothing else. An entity-change row is a row this migration's `up()` never created,
//! so its `down()` may not turn it into a schedule row naming a job that does not exist.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use migration::m20260910_000001_change_audit::{DOWN, UP};

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get::<i64>("", "n")
    .expect("count")
}

#[tokio::test]
#[serial]
async fn reverting_keeps_the_schedule_trail_and_drops_what_it_never_made() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let param = uuid::Uuid::new_v4();
    crate::common::exec_unprepared(&db, "DELETE FROM change_audit").await;
    crate::common::exec_unprepared(
        &db,
        "INSERT INTO change_audit (subject, change, old_value, new_value, changed_by) \
         VALUES ('schedule:janitor_service', 'schedule_update', '{}'::jsonb, '{}'::jsonb, 'evan')",
    )
    .await;
    crate::common::exec_unprepared(
        &db,
        &format!(
            "INSERT INTO change_audit (subject, change, old_value, new_value, changed_by) \
             VALUES ('parameter:{param}', 'parameter_update', '{{}}'::jsonb, '{{}}'::jsonb, 'evan')"
        ),
    )
    .await;

    // Read between the revert and the re-apply, so a failing assertion cannot leave the theme's
    // shared database in the pre-migration shape.
    crate::common::exec_unprepared(&db, DOWN).await;
    let rows = count(&db, "SELECT count(*)::bigint AS n FROM schedule_audit").await;
    let named = count(
        &db,
        "SELECT count(*)::bigint AS n FROM schedule_audit WHERE job_name = 'janitor_service'",
    )
    .await;
    crate::common::exec_unprepared(&db, UP).await;

    assert_eq!(
        rows, 1,
        "the restored table holds the schedule trail and nothing else"
    );
    assert_eq!(named, 1, "the schedule row keeps its job name");
}
