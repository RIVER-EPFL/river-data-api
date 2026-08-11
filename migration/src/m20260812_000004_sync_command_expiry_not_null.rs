use sea_orm_migration::prelude::*;

/// `sync_commands.expires_at` was created nullable but every writer sets it, and both the
/// core and API entities model it as non-optional. A NULL row would break the command
/// listings at query time, so the column is tightened to match the code.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "UPDATE sync_commands SET expires_at = created_at + interval '5 minutes' \
                 WHERE expires_at IS NULL",
            )
            .await?;
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE sync_commands ALTER COLUMN expires_at SET NOT NULL")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE sync_commands ALTER COLUMN expires_at DROP NOT NULL")
            .await?;
        Ok(())
    }
}
