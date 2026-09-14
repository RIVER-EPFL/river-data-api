use sea_orm_migration::prelude::*;

/// A step every calculation that needs it reads, declared rather than copied (Q156).
///
/// A step used to belong to one calculation through `calculation_formulas.tool_script_id`, and
/// `code` is unique across the table, so the second calculation computing the same step could not
/// be authored at all. A shared step is a formula row owned by no calculation, and a row here is
/// one calculation's declaration that it reads it.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS public.calculation_shared_steps (
                     id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
                     tool_script_id uuid NOT NULL
                         REFERENCES public.tool_scripts(id) ON DELETE CASCADE,
                     formula_id uuid NOT NULL
                         REFERENCES public.calculation_formulas(id) ON DELETE CASCADE,
                     created_at timestamp with time zone DEFAULT now() NOT NULL
                 );
                 CREATE UNIQUE INDEX IF NOT EXISTS calculation_shared_steps_declaration_key
                     ON public.calculation_shared_steps (tool_script_id, formula_id);
                 CREATE INDEX IF NOT EXISTS calculation_shared_steps_formula_id_idx
                     ON public.calculation_shared_steps (formula_id)",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS public.calculation_shared_steps")
            .await?;
        Ok(())
    }
}
