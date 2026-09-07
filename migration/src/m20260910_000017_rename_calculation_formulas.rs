use sea_orm_migration::prelude::*;

/// Name the table for what it holds: the formulas of a calculation, not a second parameter catalog.
///
/// Six of its columns describe a formula's place inside a calculation (`formula`,
/// `required_parameter_types`, `tool_script_id`, `ordinal`, `curve_slot`, `per_replicate`) and mean
/// nothing on a catalog row. Only the name moves: every column, foreign key, index and version pin
/// stays exactly as it is.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    ALTER TABLE IF EXISTS public.derived_parameter_definitions
        RENAME TO calculation_formulas;
";

pub const DOWN: &str = "
    ALTER TABLE IF EXISTS public.calculation_formulas
        RENAME TO derived_parameter_definitions;
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
            .execute_unprepared(DOWN)
            .await?;
        Ok(())
    }
}
