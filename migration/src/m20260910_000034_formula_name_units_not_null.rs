use sea_orm_migration::prelude::*;

/// `calculation_formulas.name` and `units` carry the same contract `parameters.name` and
/// `default_units` do, and the entity declares both `String`, so a whole-row read of a row that
/// predates the entity fails on the missing value. The columns say what the entity says.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r"
    UPDATE public.calculation_formulas SET name = code WHERE name IS NULL;
    UPDATE public.calculation_formulas SET units = '' WHERE units IS NULL;
    ALTER TABLE public.calculation_formulas
        ALTER COLUMN name SET NOT NULL,
        ALTER COLUMN units SET DEFAULT '',
        ALTER COLUMN units SET NOT NULL;
";

pub const DOWN: &str = r"
    ALTER TABLE public.calculation_formulas
        ALTER COLUMN name DROP NOT NULL,
        ALTER COLUMN units DROP DEFAULT,
        ALTER COLUMN units DROP NOT NULL;
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
