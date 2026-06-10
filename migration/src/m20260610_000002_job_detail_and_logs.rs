use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Per-job structured summary, classification, scope, and cascade linkage.
        db.execute_unprepared(
            "ALTER TABLE reprocessing_jobs \
                ADD COLUMN IF NOT EXISTS detail JSONB NOT NULL DEFAULT '{}'::jsonb, \
                ADD COLUMN IF NOT EXISTS category TEXT NOT NULL DEFAULT 'maintenance', \
                ADD COLUMN IF NOT EXISTS site_id UUID, \
                ADD COLUMN IF NOT EXISTS parent_job_id UUID \
                    REFERENCES reprocessing_jobs(id) ON DELETE SET NULL",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_reprocessing_jobs_category \
             ON reprocessing_jobs (category)",
        )
        .await?;

        // Append-only per-job timeline. `(job_id, seq)` orders lines within a job independent of
        // timestamp collisions from batch checkpoints. Cascades away with its job (so the janitor
        // prune and any retention sweep clean logs automatically).
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS reprocessing_job_logs ( \
                job_id  UUID NOT NULL REFERENCES reprocessing_jobs(id) ON DELETE CASCADE, \
                seq     BIGINT NOT NULL, \
                ts      TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
                level   TEXT NOT NULL, \
                message TEXT NOT NULL, \
                context JSONB NOT NULL DEFAULT '{}'::jsonb, \
                PRIMARY KEY (job_id, seq) \
            )",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS reprocessing_job_logs")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_reprocessing_jobs_category")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE reprocessing_jobs \
                DROP COLUMN IF EXISTS parent_job_id, \
                DROP COLUMN IF EXISTS site_id, \
                DROP COLUMN IF EXISTS category, \
                DROP COLUMN IF EXISTS detail",
        )
        .await?;
        Ok(())
    }
}
