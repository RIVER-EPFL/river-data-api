use sea_orm_migration::prelude::*;

/// Record the measurement range an instrument's manufacturer specifies.
///
/// A reading outside what the unit can measure was indistinguishable from one outside the
/// parameter's physical range, because the only bounds anywhere were the parameter's own and the
/// per-site override. The range is entered by hand: no source feed supplies one.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.sensors
                     ADD COLUMN IF NOT EXISTS range_min double precision,
                     ADD COLUMN IF NOT EXISTS range_max double precision",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.sensors
                     DROP COLUMN IF EXISTS range_min,
                     DROP COLUMN IF EXISTS range_max",
            )
            .await?;
        Ok(())
    }
}
