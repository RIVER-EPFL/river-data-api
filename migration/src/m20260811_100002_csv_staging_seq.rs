use sea_orm_migration::prelude::*;

/// Adds `seq` (file order) to csv_import_staging so the csv_import job can number replicate
/// groups deterministically: rows sharing (stream_id, time) get replicate_index 0..n-1 in the
/// order they appeared in the uploaded file, keeping re-imports idempotent.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE csv_import_staging ADD COLUMN IF NOT EXISTS seq BIGINT NOT NULL DEFAULT 0",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE csv_import_staging DROP COLUMN IF EXISTS seq")
            .await?;
        Ok(())
    }
}
