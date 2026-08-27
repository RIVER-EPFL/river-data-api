use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // One row per replicate group whose recomputed statistics disagree with what the source
        // portal stored for the same instant. The group's readings are withheld from ingest while
        // a hold is pending; an operator acknowledging it states that the trigger-computed stats
        // are what this system serves, and the group is admitted on the next re-send.
        //
        // status: pending (mismatch reported, readings withheld) -> acknowledged (operator agreed;
        // the next re-send ingests) -> consumed (the group landed). A group that later matches
        // (portal corrected itself) supersedes its stale hold.
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS replicate_audit_holds (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                stream_id UUID NOT NULL REFERENCES data_streams(id) ON DELETE CASCADE,
                group_time TIMESTAMPTZ NOT NULL,
                expected JSONB NOT NULL,
                computed JSONB NOT NULL,
                delta JSONB NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'acknowledged', 'consumed', 'superseded')),
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                acknowledged_by TEXT,
                acknowledged_at TIMESTAMPTZ
            )",
        )
        .await?;

        // One live hold per group: re-detecting the same mismatch on every sync cycle updates the
        // existing row rather than stacking duplicates.
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS replicate_audit_holds_live_uniq
             ON replicate_audit_holds (stream_id, group_time)
             WHERE status IN ('pending', 'acknowledged')",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_replicate_audit_holds_status
             ON replicate_audit_holds (status, created_at)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS replicate_audit_holds")
            .await?;
        Ok(())
    }
}
