use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // DB-backed recurring-Service scheduler (ADR 0001, Wave 2). One row per recurring job kind
        // (`job_name` = the `Job::name`/`trigger_type`). A per-replica scheduler tick selects due rows
        // `WHERE enabled AND next_run_at <= now() FOR UPDATE SKIP LOCKED`, enqueues each on the worker
        // queue keyed by (job_name, scheduled-time) so exactly one replica wins the tick, then advances
        // `next_run_at` drift-free from the scheduled time. Cadence/policies live here so they are
        // UI-editable. Additive: rows are seeded at startup from `Job::default_schedule`, no backfill.
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS schedules ( \
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
                 job_name TEXT NOT NULL UNIQUE, \
                 enabled BOOLEAN NOT NULL DEFAULT true, \
                 next_run_at TIMESTAMPTZ, \
                 interval_seconds BIGINT, \
                 overlap_policy TEXT, \
                 catchup_policy TEXT, \
                 tunables JSONB NOT NULL DEFAULT '{}'::jsonb, \
                 last_enqueued_at TIMESTAMPTZ, \
                 updated_by TEXT, \
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now() \
             )",
        )
        .await?;

        // Due-row scan path: enabled rows that have come due. Matches the scheduler's
        // `WHERE enabled AND next_run_at <= now()` claim.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_schedules_due \
             ON schedules (next_run_at) WHERE enabled",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_schedules_due")
            .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS schedules")
            .await?;
        Ok(())
    }
}
