use sea_orm_migration::prelude::*;

/// Let one formula of a calculation declare the curve slot it corrects with.
///
/// The portal applies a standard curve per output, not per calculation: Chl a corrects the acid
/// and no-acid readings with two different curves in one row. The slot's coefficients reach the
/// formula as the variables `curve_slope` and `curve_intercept`.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    ALTER TABLE public.derived_parameter_definitions
        ADD COLUMN IF NOT EXISTS curve_slot character varying(64);
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.derived_parameter_definitions DROP COLUMN IF EXISTS curve_slot;",
            )
            .await?;
        Ok(())
    }
}
