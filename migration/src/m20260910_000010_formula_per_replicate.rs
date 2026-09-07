use sea_orm_migration::prelude::*;

/// Let one formula of a calculation evaluate once per replicate index.
///
/// Four of the portal's calculators are two-stage: pCO2 computes six columns per replicate over A
/// and B, DIC two, Nutrients three, Chl a seven. Each stage-1 column is a stored parameter in its
/// own right (Q95), one reading per replicate index, with the `samples` trigger deriving the mean
/// and standard deviation as it does for every other replicate family.
///
/// The column names the formula variable whose replicate vector sets the width, so the output's
/// replicate identity is inherited from the input it was computed from rather than assigned by the
/// calculation.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    ALTER TABLE public.derived_parameter_definitions
        ADD COLUMN IF NOT EXISTS per_replicate character varying(64);
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
                "ALTER TABLE public.derived_parameter_definitions \
                 DROP COLUMN IF EXISTS per_replicate;",
            )
            .await?;
        Ok(())
    }
}
