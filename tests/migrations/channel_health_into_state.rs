//! Scenario: a database carried forward holds a probed `notification_channel_health` row.
//!
//! Expected behaviour: the probe's verdict, its message and its time arrive in
//! `notification_state` under the `channel_health` kind, and the one-row table is gone.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use migration::m20260908_000008_channel_health_into_state::UP;

/// The shape the table had before the migration, with one probed row in it.
async fn restore_pre_migration_shape(db: &DatabaseConnection) {
    crate::common::exec_unprepared(
        db,
        "CREATE TABLE IF NOT EXISTS notification_channel_health ( \
             channel text PRIMARY KEY, healthy boolean NOT NULL, detail text, \
             checked_at timestamptz NOT NULL DEFAULT now())",
    )
    .await;
    crate::common::exec_unprepared(
        db,
        "INSERT INTO notification_channel_health (channel, healthy, detail, checked_at) \
         VALUES ('web_push', false, 'endpoint refused', '2026-09-01T10:00:00Z')",
    )
    .await;
    crate::common::exec_unprepared(
        db,
        "ALTER TABLE notification_state DROP COLUMN IF EXISTS detail",
    )
    .await;
    crate::common::exec_unprepared(
        db,
        "DELETE FROM notification_state WHERE kind = 'channel_health'",
    )
    .await;
}

#[tokio::test]
#[serial]
async fn a_probed_channel_arrives_in_the_ledger() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    restore_pre_migration_shape(&db).await;

    crate::common::exec_unprepared(&db, UP).await;

    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT state, detail, last_notified_at::text AS at FROM notification_state \
              WHERE kind = 'channel_health' AND subject_key = 'web_push'"
                .to_string(),
        ))
        .await
        .expect("query")
        .expect("the migrated row");
    assert_eq!(
        row.try_get::<String>("", "state").expect("state"),
        "unhealthy"
    );
    assert_eq!(
        row.try_get::<String>("", "detail").expect("detail"),
        "endpoint refused"
    );
    assert!(
        row.try_get::<String>("", "at")
            .expect("last_notified_at")
            .starts_with("2026-09-01 10:00:00"),
        "the probe's own time is kept"
    );

    let table_gone: bool = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT to_regclass('public.notification_channel_health') IS NULL AS gone".to_string(),
        ))
        .await
        .expect("query")
        .expect("a row")
        .try_get("", "gone")
        .expect("gone");
    assert!(table_gone, "the one-row table is dropped");

    crate::common::cleanup_test_db(&db).await;
}
