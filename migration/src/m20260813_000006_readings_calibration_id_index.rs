use sea_orm_migration::prelude::*;

/// Indexes the referencing side of `readings.calibration_id`.
///
/// The column is declared `UUID REFERENCES sensor_calibrations(id)` with no `ON DELETE`
/// (`m20260325_000001_init`), so every delete of a calibration runs a referencing-side check
/// against the readings hypertable. No index covered that column: the seven that existed are
/// `readings_pkey`, `readings_time_idx`, `idx_readings_site_param_time`, `idx_readings_stream_time`,
/// `idx_readings_sample_id`, `idx_readings_sensor_time` and `idx_readings_standard_curve_id`, so
/// each check was a full scan. Measured on dev, deleting the auto-minted calibrations took 3m10s
/// without this index and roughly 33s with it, and the index itself builds in under a second.
///
/// Its own migration, ordered ahead of the delete that needs it. The split buys legibility and
/// ordering, not isolation: on Postgres `sea-orm-migration` opens one transaction, runs every
/// pending migration inside it and commits once, so a failure anywhere in a deploy's batch rolls
/// this index back along with everything else in that batch.
///
/// Partial on `IS NOT NULL`, matching `idx_readings_standard_curve_id` and `idx_readings_sample_id`:
/// the foreign key check probes `calibration_id = $1`, which implies the predicate, so a partial
/// index serves it. That same shared transaction rules out `CREATE INDEX CONCURRENTLY`, which
/// Postgres refuses inside a transaction block, so this is a plain `CREATE INDEX` and takes a lock
/// that blocks writes to `readings` while it builds — under a second on dev.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_readings_calibration_id \
                 ON readings (calibration_id) WHERE calibration_id IS NOT NULL;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP INDEX IF EXISTS idx_readings_calibration_id;")
            .await?;
        Ok(())
    }
}
