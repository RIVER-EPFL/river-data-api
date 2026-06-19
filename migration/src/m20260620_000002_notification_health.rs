use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Persisted heartbeat: one row per channel, upserted by the background health probe. The admin
        // health endpoint reads the latest state so the dashboard shows whether each channel is
        // actually reachable, with a last-checked time — like sync_services.last_heartbeat.
        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS notification_channel_health (
                channel    TEXT PRIMARY KEY,
                healthy    BOOLEAN NOT NULL,
                detail     TEXT,
                checked_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS notification_channel_health")
            .await?;
        Ok(())
    }
}
