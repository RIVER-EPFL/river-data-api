use sea_orm_migration::prelude::*;

/// Record a calculation's decommission: who, when and why. A decommissioned calculation is never
/// enabled, so every reader of `enabled` stops it at every site without a change of its own.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
ALTER TABLE public.tool_scripts
    ADD COLUMN IF NOT EXISTS decommissioned_at timestamptz,
    ADD COLUMN IF NOT EXISTS decommissioned_by text,
    ADD COLUMN IF NOT EXISTS decommission_reason text;
ALTER TABLE public.tool_scripts
    ADD CONSTRAINT tool_scripts_decommission_complete CHECK (
        (decommissioned_at IS NULL AND decommissioned_by IS NULL AND decommission_reason IS NULL)
        OR (decommissioned_at IS NOT NULL AND decommissioned_by IS NOT NULL
            AND decommission_reason IS NOT NULL)
    ),
    ADD CONSTRAINT tool_scripts_decommissioned_not_enabled CHECK (
        decommissioned_at IS NULL OR NOT enabled
    );
";

const DOWN: &str = r"
ALTER TABLE public.tool_scripts
    DROP CONSTRAINT IF EXISTS tool_scripts_decommissioned_not_enabled,
    DROP CONSTRAINT IF EXISTS tool_scripts_decommission_complete,
    DROP COLUMN IF EXISTS decommission_reason,
    DROP COLUMN IF EXISTS decommissioned_by,
    DROP COLUMN IF EXISTS decommissioned_at;
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
