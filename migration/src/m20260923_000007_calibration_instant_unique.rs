use sea_orm_migration::prelude::*;

/// One curve per instrument channel per opening instant. Two curves opening together leave one with
/// an empty window, and a curve naming no parameter applies to every channel, so two of those
/// collide as well.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
CREATE UNIQUE INDEX IF NOT EXISTS sensor_calibrations_channel_opening_key
    ON public.sensor_calibrations (sensor_id, parameter_id, valid_from) NULLS NOT DISTINCT;
";

const DOWN: &str = r"
DROP INDEX IF EXISTS public.sensor_calibrations_channel_opening_key;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }
}
