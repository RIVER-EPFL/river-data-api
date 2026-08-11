use sea_orm_migration::prelude::*;

/// Adds `paused` to sync_services so an operator's pause survives service restarts,
/// and `measurement_type` to alarm_events so grab-caused and sensor-caused alarms
/// are distinguishable.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE sync_services ADD COLUMN IF NOT EXISTS paused BOOLEAN NOT NULL DEFAULT FALSE",
            )
            .await?;
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE alarm_events ADD COLUMN IF NOT EXISTS measurement_type VARCHAR(32)",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE sync_services DROP COLUMN IF EXISTS paused")
            .await?;
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE alarm_events DROP COLUMN IF EXISTS measurement_type")
            .await?;
        Ok(())
    }
}
