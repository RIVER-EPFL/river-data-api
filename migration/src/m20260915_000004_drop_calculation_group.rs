use sea_orm_migration::prelude::*;

/// Drop `tool_scripts.parameter_group_id`, its unique index and its foreign key.
///
/// A parameter group organises the grid's columns (Q169); a calculation reads and writes
/// parameters by code and belongs to no group. The column also held one calculation per group,
/// which refused a second one against the same members.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.tool_scripts DROP COLUMN IF EXISTS parameter_group_id",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.tool_scripts
                     ADD COLUMN IF NOT EXISTS parameter_group_id uuid
                     REFERENCES public.parameter_groups(id)",
            )
            .await?;
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_tool_scripts_parameter_group
                     ON public.tool_scripts (parameter_group_id)
                  WHERE parameter_group_id IS NOT NULL",
            )
            .await?;
        Ok(())
    }
}
