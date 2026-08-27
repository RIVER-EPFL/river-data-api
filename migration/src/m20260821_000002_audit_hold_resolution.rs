use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // The decision record for a reviewed hold: what was done ("accept_ours" or
        // "flag_replicates" with the indexes and reason), with prior actions kept under
        // "history" so a reopened and re-resolved hold retains its full decision trail.
        db.execute_unprepared(
            "ALTER TABLE replicate_audit_holds ADD COLUMN IF NOT EXISTS resolution JSONB",
        )
        .await?;

        // 'remediated': the operator flagged specific replicates and the sample statistics
        // recomputed from the rest. The legacy statuses stay valid for history; nothing
        // produces use_portal, use_manual or consumed any more.
        db.execute_unprepared(
            "ALTER TABLE replicate_audit_holds
             DROP CONSTRAINT IF EXISTS replicate_audit_holds_status_check",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE replicate_audit_holds
             ADD CONSTRAINT replicate_audit_holds_status_check
             CHECK (status IN ('pending', 'deferred', 'acknowledged', 'remediated',
                               'superseded', 'use_portal', 'use_manual', 'consumed'))",
        )
        .await?;

        // A use_portal / use_manual decision could only take effect through the removed
        // value-synthesis path, so it returns to review under the new resolution modes.
        db.execute_unprepared(
            "UPDATE replicate_audit_holds SET status = 'pending', manual_value = NULL
             WHERE status IN ('use_portal', 'use_manual')",
        )
        .await?;

        // Only open holds are unique per group now: acknowledged and remediated are terminal
        // decisions the ingest gate leaves standing rather than statuses awaiting a re-send.
        db.execute_unprepared("DROP INDEX IF EXISTS replicate_audit_holds_live_uniq")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX replicate_audit_holds_live_uniq
             ON replicate_audit_holds (stream_id, group_time)
             WHERE status IN ('pending', 'deferred')",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "UPDATE replicate_audit_holds SET status = 'acknowledged' WHERE status = 'remediated'",
        )
        .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS replicate_audit_holds_live_uniq")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX replicate_audit_holds_live_uniq
             ON replicate_audit_holds (stream_id, group_time)
             WHERE status IN ('pending', 'deferred', 'acknowledged', 'use_portal', 'use_manual')",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE replicate_audit_holds
             DROP CONSTRAINT IF EXISTS replicate_audit_holds_status_check",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE replicate_audit_holds
             ADD CONSTRAINT replicate_audit_holds_status_check
             CHECK (status IN ('pending', 'deferred', 'acknowledged', 'use_portal', 'use_manual',
                               'consumed', 'superseded'))",
        )
        .await?;
        db.execute_unprepared("ALTER TABLE replicate_audit_holds DROP COLUMN IF EXISTS resolution")
            .await?;
        Ok(())
    }
}
