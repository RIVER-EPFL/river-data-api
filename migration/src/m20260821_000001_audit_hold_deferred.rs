use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // 'deferred': a mismatch detected on an unpaired stream. The readings are admitted (they
        // are unattributed and served nowhere), the cursor advances, and the hold waits out of the
        // review queue until the stream is paired, when it flips to 'pending' and is resolved in
        // place against the stored rows.
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

        db.execute_unprepared("DROP INDEX IF EXISTS replicate_audit_holds_live_uniq")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX replicate_audit_holds_live_uniq
             ON replicate_audit_holds (stream_id, group_time)
             WHERE status IN ('pending', 'deferred', 'acknowledged', 'use_portal', 'use_manual')",
        )
        .await?;

        // Holds minted as 'pending' on streams that were never paired belong to the deferred
        // class this migration introduces.
        db.execute_unprepared(
            "UPDATE replicate_audit_holds h SET status = 'deferred'
             FROM data_streams ds
             WHERE ds.id = h.stream_id AND h.status = 'pending'
               AND ds.site_parameter_id IS NULL",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "UPDATE replicate_audit_holds SET status = 'pending' WHERE status = 'deferred'",
        )
        .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS replicate_audit_holds_live_uniq")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX replicate_audit_holds_live_uniq
             ON replicate_audit_holds (stream_id, group_time)
             WHERE status IN ('pending', 'acknowledged', 'use_portal', 'use_manual')",
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
             CHECK (status IN ('pending', 'acknowledged', 'use_portal', 'use_manual',
                               'consumed', 'superseded'))",
        )
        .await?;
        Ok(())
    }
}
