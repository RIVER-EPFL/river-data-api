use sea_orm_migration::prelude::*;

/// Adds `seq` (file order) to csv_import_staging so replicate groups are numbered deterministically.
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
