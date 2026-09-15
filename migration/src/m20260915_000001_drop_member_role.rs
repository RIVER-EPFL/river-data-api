use sea_orm_migration::prelude::*;

/// Drop `parameter_group_members.role`.
///
/// What a parameter is to the calculations is read off the calculations (Q135), so the stored
/// column was a second answer to the same question and disagreed with the derived one.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.parameter_group_members DROP COLUMN IF EXISTS role",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.parameter_group_members
                     ADD COLUMN IF NOT EXISTS role text NOT NULL DEFAULT 'entry_only',
                     ADD CONSTRAINT parameter_group_members_role_check
                         CHECK (role = ANY (ARRAY['measured'::text, 'entry_only'::text, 'output'::text]))",
            )
            .await?;
        Ok(())
    }
}
