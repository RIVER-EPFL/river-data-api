use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Append-only trail of operator edits to `schedules` (Stage D). One row per accepted PATCH,
        // capturing the editable-field snapshot before (`old_value`) and after (`new_value`) so a
        // cadence/tunables change is attributable and reversible by inspection. `changed_by` is the
        // request principal (Keycloak email or `token:<id>`); null only when no identity was resolved.
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS schedule_audit ( \
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
                 job_name TEXT NOT NULL, \
                 changed_by TEXT, \
                 old_value JSONB, \
                 new_value JSONB, \
                 changed_at TIMESTAMPTZ NOT NULL DEFAULT now() \
             )",
        )
        .await?;

        // History read path: newest edits for one job first (the `/schedules/{job_name}/audit` feed).
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_schedule_audit_job_changed \
             ON schedule_audit (job_name, changed_at DESC)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_schedule_audit_job_changed")
            .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS schedule_audit")
            .await?;
        Ok(())
    }
}
