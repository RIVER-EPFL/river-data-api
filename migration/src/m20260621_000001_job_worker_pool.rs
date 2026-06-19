use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Claim-based multi-replica worker pool (ADR 0001). A job row is leased by a worker
        // (`owner` + `lease_expires_at`, fenced by the monotonic `lease_epoch`), carries the inputs
        // needed to run or replay it (`params`), is cancellable from any replica (`cancel_requested`),
        // and is durably retried by becoming claimable again at `next_attempt_at`. `dedupe_key` makes
        // enqueues idempotent — the scheduler keys it on (job, scheduled time) so two replicas racing
        // a tick can't both enqueue the same run.
        db.execute_unprepared(
            "ALTER TABLE reprocessing_jobs \
                ADD COLUMN IF NOT EXISTS owner TEXT, \
                ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ, \
                ADD COLUMN IF NOT EXISTS lease_epoch BIGINT NOT NULL DEFAULT 0, \
                ADD COLUMN IF NOT EXISTS cancel_requested BOOLEAN NOT NULL DEFAULT false, \
                ADD COLUMN IF NOT EXISTS params JSONB NOT NULL DEFAULT '{}'::jsonb, \
                ADD COLUMN IF NOT EXISTS next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
                ADD COLUMN IF NOT EXISTS dedupe_key TEXT",
        )
        .await?;

        // At most one live job per dedupe_key — the idempotent-enqueue guarantee the scheduler and
        // any deduplicated trigger rely on.
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_reprocessing_jobs_dedupe_key \
             ON reprocessing_jobs (dedupe_key) WHERE dedupe_key IS NOT NULL",
        )
        .await?;

        // Claim path: pending rows that have come due (`next_attempt_at <= now()`).
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_reprocessing_jobs_claimable \
             ON reprocessing_jobs (next_attempt_at) WHERE status = 'pending'",
        )
        .await?;

        // Reaper path: running rows whose lease has expired (a dead or CPU-throttled worker).
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_reprocessing_jobs_lease \
             ON reprocessing_jobs (lease_expires_at) WHERE status = 'running'",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_reprocessing_jobs_lease")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_reprocessing_jobs_claimable")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS uq_reprocessing_jobs_dedupe_key")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE reprocessing_jobs \
                DROP COLUMN IF EXISTS dedupe_key, \
                DROP COLUMN IF EXISTS next_attempt_at, \
                DROP COLUMN IF EXISTS params, \
                DROP COLUMN IF EXISTS cancel_requested, \
                DROP COLUMN IF EXISTS lease_epoch, \
                DROP COLUMN IF EXISTS lease_expires_at, \
                DROP COLUMN IF EXISTS owner",
        )
        .await?;
        Ok(())
    }
}
