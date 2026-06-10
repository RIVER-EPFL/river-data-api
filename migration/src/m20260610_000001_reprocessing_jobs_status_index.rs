use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // The startup reconciliation sweep and the in-flight guard both filter `reprocessing_jobs`
        // by `status` alone. Index it so those queries stay fast as job history grows.
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_reprocessing_jobs_status \
                 ON reprocessing_jobs (status)",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP INDEX IF EXISTS idx_reprocessing_jobs_status")
            .await?;
        Ok(())
    }
}
