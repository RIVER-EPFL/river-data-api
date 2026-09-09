use sea_orm_migration::prelude::*;

/// A formula whose value is a step of the calculation, not a measurement of anything.
///
/// The portal's pCO2 holds the pressure choice and the Henry term inside the function; written as
/// formulas they would each mint a catalog parameter, appear in the parameter list and the CSV
/// exports, and be offered for saving. An intermediate mints none: it is evaluated, handed to the
/// formulas after it and reported in the run, and stored nowhere.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r"
    ALTER TABLE public.calculation_formulas
        ADD COLUMN IF NOT EXISTS intermediate boolean NOT NULL DEFAULT false;
";

pub const DOWN: &str = r"
    ALTER TABLE public.calculation_formulas DROP COLUMN IF EXISTS intermediate;
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
