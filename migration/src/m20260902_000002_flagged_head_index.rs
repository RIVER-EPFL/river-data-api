use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // The per-parameter extents draw their head from sources that exclude flagged rows, so a
        // series whose oldest readings are flagged reports a start after them. The head probe
        // reads only flagged rows, which are a small fraction of the table; this is what keeps it
        // off an unbounded hypertable scan.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_readings_flagged_site_param_time
             ON readings (site_id, parameter_id, time)
             WHERE is_flagged",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP INDEX IF EXISTS idx_readings_flagged_site_param_time")
            .await?;
        Ok(())
    }
}
