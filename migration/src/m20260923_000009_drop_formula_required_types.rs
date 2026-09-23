use sea_orm_migration::prelude::*;

/// Drop `calculation_formulas.required_parameter_types`, a column no entity, writer or reader names:
/// always NULL, it read as a constraint on a formula's inputs that nothing enforces.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.calculation_formulas DROP COLUMN IF EXISTS required_parameter_types",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.calculation_formulas ADD COLUMN IF NOT EXISTS required_parameter_types jsonb",
            )
            .await?;
        Ok(())
    }
}
