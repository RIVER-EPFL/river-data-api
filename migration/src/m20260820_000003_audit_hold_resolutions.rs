use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Tri-modal hold resolution: alongside 'acknowledged' (serve the recomputed statistics),
        // an operator can resolve 'use_portal' (serve the portal's stored mean) or 'use_manual'
        // (serve an entered value, held in manual_value). All three are live until the group's
        // next re-send applies them and consumes the hold, so the partial unique index covers
        // them too.
        db.execute_unprepared(
            "ALTER TABLE replicate_audit_holds
             ADD COLUMN IF NOT EXISTS manual_value DOUBLE PRECISION",
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

        db.execute_unprepared("DROP INDEX IF EXISTS replicate_audit_holds_live_uniq")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX replicate_audit_holds_live_uniq
             ON replicate_audit_holds (stream_id, group_time)
             WHERE status IN ('pending', 'acknowledged', 'use_portal', 'use_manual')",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS replicate_audit_holds_live_uniq")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX replicate_audit_holds_live_uniq
             ON replicate_audit_holds (stream_id, group_time)
             WHERE status IN ('pending', 'acknowledged')",
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
             CHECK (status IN ('pending', 'acknowledged', 'consumed', 'superseded'))",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE replicate_audit_holds DROP COLUMN IF EXISTS manual_value",
        )
        .await?;
        Ok(())
    }
}
