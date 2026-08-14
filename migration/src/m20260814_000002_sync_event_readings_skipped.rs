use sea_orm_migration::prelude::*;

/// Adds `readings_skipped` to sync_events. Readings dropped by ingest admission are
/// otherwise only visible in the API process log, so a stream losing rows every cycle
/// leaves no queryable trace.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE sync_events ADD COLUMN IF NOT EXISTS readings_skipped BIGINT NOT NULL DEFAULT 0",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE sync_events DROP COLUMN IF EXISTS readings_skipped")
            .await?;
        Ok(())
    }
}
