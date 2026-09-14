use sea_orm_migration::prelude::*;

/// The 6-hour and 12-hour rollups, of the same shape as the four the baseline builds.
///
/// The portals serve four resolutions from their own files; 24H is `readings_daily` and the two
/// between an hour and a day had no view here.
#[derive(DeriveMigrationName)]
pub struct Migration;

const VIEW: &str = r#"
CREATE MATERIALIZED VIEW readings_{name}
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT
    time_bucket('{width}', time) AS bucket,
    site_id,
    parameter_id, sensor_id,
    AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
    MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
    MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
    COUNT(*) AS count,
    STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value,
    SUM(COALESCE(calibrated_value, raw_value)) AS sum_value,
    SUM(COALESCE(calibrated_value, raw_value) * COALESCE(calibrated_value, raw_value)) AS sum_sq_value
FROM readings
WHERE site_id IS NOT NULL AND replicate_index = 0 AND (is_flagged IS NOT TRUE) AND measurement_type IS DISTINCT FROM 'spot'
GROUP BY time_bucket('{width}', time), site_id, parameter_id, sensor_id
WITH NO DATA;

SELECT add_continuous_aggregate_policy('readings_{name}',
    start_offset => NULL, end_offset => INTERVAL '{width}', schedule_interval => INTERVAL '1 hour', buckets_per_batch => 0, if_not_exists => TRUE);
"#;

fn rollup(name: &str, width: &str) -> String {
    VIEW.replace("{name}", name).replace("{width}", width)
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(&rollup("six_hourly", "6 hours"))
            .await?;
        db.execute_unprepared(&rollup("twelve_hourly", "12 hours"))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP MATERIALIZED VIEW IF EXISTS readings_twelve_hourly;
                 DROP MATERIALIZED VIEW IF EXISTS readings_six_hourly;",
            )
            .await?;
        Ok(())
    }
}
