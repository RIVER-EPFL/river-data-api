use sea_orm_migration::prelude::*;

/// Widen the readings chunk interval from 7 to 90 days, forward-only: existing chunks keep
/// their ranges. Query planning over the hypertable costs per chunk considered, and 7-day
/// chunks put ten years of data at several hundred chunks; 90 days holds a year in four or
/// five. Compression still runs per chunk once its whole range passes the 30-day policy, so
/// recent data stays uncompressed for up to ~4 months, which DML on the live window benefits
/// from.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "SELECT set_chunk_time_interval('readings', INTERVAL '90 days')",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("SELECT set_chunk_time_interval('readings', INTERVAL '7 days')")
            .await?;
        Ok(())
    }
}
