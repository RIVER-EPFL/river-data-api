use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // The alarm-events history feed orders by `last_seen_at DESC` and now also filters by time
        // range. Index the order/filter column so paginated history queries stay fast as the table
        // grows with backfilled episodes.
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_alarm_events_last_seen \
                 ON alarm_events (last_seen_at DESC)",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP INDEX IF EXISTS idx_alarm_events_last_seen")
            .await?;
        Ok(())
    }
}
