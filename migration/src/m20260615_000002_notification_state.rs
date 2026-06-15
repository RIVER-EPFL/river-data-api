use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Dedup + recovery state for signal-based alerts that aren't alarm_events (stale data, sync
        // failures, battery forecast). One row per (kind, subject); `state` distinguishes a firing
        // condition from a recovered one, and `last_notified_at` drives re-notify suppression.
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                CREATE TABLE IF NOT EXISTS notification_state (
                    kind             TEXT NOT NULL,
                    subject_key      TEXT NOT NULL,
                    state            TEXT NOT NULL DEFAULT 'firing',
                    last_notified_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (kind, subject_key)
                )
                "#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS notification_state")
            .await?;
        Ok(())
    }
}
