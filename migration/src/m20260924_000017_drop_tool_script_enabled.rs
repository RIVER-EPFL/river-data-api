use sea_orm_migration::prelude::*;

/// A calculation has no global switch (Q326): it stops at a site when it is removed there, and
/// everywhere when it is decommissioned.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
ALTER TABLE public.tool_scripts
    DROP CONSTRAINT IF EXISTS tool_scripts_decommissioned_not_enabled,
    DROP COLUMN IF EXISTS enabled;
";

const DOWN: &str = r"
ALTER TABLE public.tool_scripts
    ADD COLUMN enabled boolean NOT NULL DEFAULT true;
UPDATE public.tool_scripts SET enabled = false WHERE decommissioned_at IS NOT NULL;
ALTER TABLE public.tool_scripts
    ADD CONSTRAINT tool_scripts_decommissioned_not_enabled CHECK (
        decommissioned_at IS NULL OR NOT enabled
    );
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
