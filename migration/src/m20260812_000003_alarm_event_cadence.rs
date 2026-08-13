use sea_orm_migration::prelude::*;

/// Makes alarm_events cadence-aware: grab (spot) breaches are evaluated as their own series,
/// so one open event per (site, parameter, measurement_type) instead of per (site, parameter).
/// Existing events predate the distinction and were sensor-driven; backfill 'continuous'.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "UPDATE alarm_events SET measurement_type = 'continuous' WHERE measurement_type IS NULL",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_events ALTER COLUMN measurement_type SET DEFAULT 'continuous'",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_events ALTER COLUMN measurement_type SET NOT NULL",
        )
        .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS uq_alarm_events_open")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_events_open \
             ON alarm_events (site_id, parameter_id, measurement_type) WHERE resolved_at IS NULL",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS uq_alarm_events_open")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_events_open \
             ON alarm_events (site_id, parameter_id) WHERE resolved_at IS NULL",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_events ALTER COLUMN measurement_type DROP NOT NULL",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_events ALTER COLUMN measurement_type DROP DEFAULT",
        )
        .await?;
        Ok(())
    }
}
