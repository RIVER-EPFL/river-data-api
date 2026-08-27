use sea_orm_migration::prelude::*;

/// Partial indexes over the spot subset of `readings`.
///
/// Every serving read is split into a continuous arm (`replicate_index = 0 AND measurement_type
/// IS DISTINCT FROM 'spot'`) and a spot arm (`measurement_type = 'spot'`), but no index carries
/// `measurement_type`, so the spot arm re-reads its whole slot or sensor range through
/// `idx_readings_site_param_time` / `idx_readings_sensor_time` and filters row by row. The spot
/// population is a small fraction of the table (grab and lab entries against a continuous logger
/// record), which is exactly the shape a partial index serves: the index holds only the rows the
/// arm can return.
///
/// Two probe shapes exist in the reworked queries, hence two indexes:
/// - `(site_id, parameter_id, time)`: the alarm evaluators and episode builder, the private and
///   public site series, and the notification triggers all probe spot by slot.
/// - `(sensor_id, time)`: the sensor diagnostic series and the calibration window count/point
///   queries probe spot by sensor. Nothing probes spot by `stream_id` alone (the
///   `DISTINCT ON (stream_id, time)` dedupe sorts rows already found by slot or sensor), so no
///   stream-keyed index is created.
///
/// Plain `CREATE INDEX`, matching every other index migration here: `sea-orm-migration` runs the
/// batch in one transaction, which rules out `CONCURRENTLY`.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_readings_spot_site_param_time \
             ON readings (site_id, parameter_id, time) WHERE measurement_type = 'spot';",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_readings_spot_sensor_time \
             ON readings (sensor_id, time) WHERE measurement_type = 'spot';",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_readings_spot_site_param_time;")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_readings_spot_sensor_time;")
            .await?;
        Ok(())
    }
}
