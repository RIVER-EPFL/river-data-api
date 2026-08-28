use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Where an instrument came from, for the lab instruments a sync service finds-or-creates
        // per (source_system, curve label). Devices with a real serial keep NULLs. Until now the
        // provenance was encoded in `serial_number` as "{source}:{label}" and deduped by
        // `idx_sensors_serial_unique`, which forces every lookup to parse a serial and leaves a
        // portal instrument indistinguishable from a device whose serial contains a colon.
        db.execute_unprepared(
            "ALTER TABLE sensors
             ADD COLUMN IF NOT EXISTS source_system TEXT,
             ADD COLUMN IF NOT EXISTS source_key TEXT",
        )
        .await?;

        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS sensors_provenance_uniq
             ON sensors (source_system, source_key)
             WHERE source_system IS NOT NULL AND source_key IS NOT NULL",
        )
        .await?;

        // Backfill the instruments that predate the columns. Their source is recoverable exactly
        // rather than guessed: every one of them owns the curves a sync service registered, and
        // `standard_curves.source_system` names the source directly. The source_key is the serial
        // verbatim, which is what the upsert already keys on. A sensor whose curves disagree about
        // the source is left NULL instead of picking one.
        db.execute_unprepared(
            "UPDATE sensors s
             SET source_system = c.source_system,
                 source_key = s.serial_number
             FROM (
                 SELECT sensor_id, MIN(source_system) AS source_system
                 FROM standard_curves
                 WHERE source_system IS NOT NULL
                 GROUP BY sensor_id
                 HAVING COUNT(DISTINCT source_system) = 1
             ) c
             WHERE c.sensor_id = s.id
               AND s.serial_number IS NOT NULL
               AND s.source_system IS NULL",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS sensors_provenance_uniq")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE sensors
             DROP COLUMN IF EXISTS source_system,
             DROP COLUMN IF EXISTS source_key",
        )
        .await?;
        Ok(())
    }
}
