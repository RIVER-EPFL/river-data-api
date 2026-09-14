use sea_orm_migration::prelude::*;

/// Record the source calculation behind a group member the portal computes.
///
/// `parameter_group_members.role` says a column is an output; nothing said what wrote it, so a
/// CNET column arriving as `role = 'output'` lost the portal function and its inputs at the
/// stream metadata and could not be told from an output whose formula set has been authored.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.parameter_group_members
                     ADD COLUMN IF NOT EXISTS source_calculation jsonb",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.parameter_group_members DROP COLUMN IF EXISTS source_calculation",
            )
            .await?;
        Ok(())
    }
}
