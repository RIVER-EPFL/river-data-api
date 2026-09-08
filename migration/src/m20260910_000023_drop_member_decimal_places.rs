use sea_orm_migration::prelude::*;

/// Decimal places are declared per slot, not per group member (Q120).
///
/// Rounding is presentation: full resolution is stored, and how much of it a form or the public API
/// shows is `site_parameters.decimal_places`, else the platform default. A second declaration on the
/// group member was a third answer to that, so it goes.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r"
    ALTER TABLE public.parameter_group_members DROP COLUMN IF EXISTS decimal_places;
";

pub const DOWN: &str = r"
    ALTER TABLE public.parameter_group_members ADD COLUMN IF NOT EXISTS decimal_places integer;
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
