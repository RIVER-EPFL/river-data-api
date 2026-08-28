use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Arrival time of the stored row. Overwrites re-stamp it; NULL means the row predates
        // tracking and provenance falls back to receipt or sync-cycle granularity. Two
        // statements: a volatile default cannot ride ADD COLUMN on a columnstore-enabled
        // hypertable, while the bare column and a later SET DEFAULT (new inserts only) both can.
        db.execute_unprepared("ALTER TABLE readings ADD COLUMN IF NOT EXISTS ingested_at TIMESTAMPTZ")
            .await?;
        db.execute_unprepared("ALTER TABLE readings ALTER COLUMN ingested_at SET DEFAULT NOW()")
            .await?;
        // The visits table counts a visit's readings by event; without this the per-event
        // subquery walks the hypertable.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_readings_collection_event
             ON readings (collection_event_id)
             WHERE collection_event_id IS NOT NULL",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "SET timescaledb.max_tuples_decompressed_per_dml_transaction = 0",
        )
        .await?;
        db.execute_unprepared("ALTER TABLE readings DROP COLUMN IF EXISTS ingested_at")
            .await?;
        Ok(())
    }
}
