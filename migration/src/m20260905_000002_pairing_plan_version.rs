use sea_orm_migration::prelude::*;

/// A pairing plan's `entries` document is rewritten whole by every edit, so two reviewers on one
/// draft need a version to write against: the second write of a pair carrying the same version is
/// refused rather than carrying the first's entries back over the first's decisions.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE pairing_plans ADD COLUMN IF NOT EXISTS version integer NOT NULL DEFAULT 0",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE pairing_plans DROP COLUMN IF EXISTS version")
            .await?;
        Ok(())
    }
}
