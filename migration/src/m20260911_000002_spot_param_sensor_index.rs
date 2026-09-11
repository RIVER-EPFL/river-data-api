use sea_orm_migration::prelude::*;

/// Let "the instruments last used for this parameter" be answered by an index walk.
///
/// A field instrument's readings are `spot`, which the rollups exclude, so the only place the
/// answer exists is `readings`. The spot indexes before this one lead with `sensor_id` or with
/// `site_id, parameter_id`, so a `MAX(time) GROUP BY sensor_id` filtered by parameter could not
/// read the instrument and the instant from one index.
///
/// Measured by M204 on 15.2M continuous plus 62,640 spot readings over 42 chunks: the grouped
/// query goes from 4.8 ms to 2.2 ms of execution, and from an index scan to an index-only scan.
/// Planning is 25 ms either way, the chunk-count tax this cannot touch, which is why the item
/// stops here and adds no maintained last-use column.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_readings_spot_param_sensor_time
                     ON public.readings (parameter_id, sensor_id, \"time\" DESC)
                  WHERE measurement_type = 'spot'",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP INDEX IF EXISTS idx_readings_spot_param_sensor_time")
            .await?;
        Ok(())
    }
}
