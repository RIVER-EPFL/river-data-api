use sea_orm_migration::prelude::*;

/// Add `kind` to the live-hold unique index.
///
/// Three writers upsert on `(stream_id, group_time)`: the statistics audit, the windowed diff's
/// `source_modified` and `brake_fired` holds, and the stripped-curve-claim hold. Without a `kind`
/// term they collide, so an instant that both disagrees statistically and carries a curated row
/// the source changed keeps one row whose evidence is whichever writer ran last. The statistics
/// hold's `{index, value}` pairs are what a `flag` resolution addresses replicates by, and losing
/// them turns that resolution into a 400 blaming a legacy schema.
///
/// Existing rows collapse under the wider key rather than conflict with it, so no data moves.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS replicate_audit_holds_live_uniq")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX replicate_audit_holds_live_uniq
             ON replicate_audit_holds (stream_id, group_time, kind)
             WHERE status IN ('pending', 'deferred')",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // Narrowing the key again needs the duplicates it forbids gone; the newest row per group
        // is the one the old index would have kept.
        db.execute_unprepared(
            "DELETE FROM replicate_audit_holds h
             WHERE h.status IN ('pending', 'deferred')
               AND h.stream_id IS NOT NULL
               AND EXISTS (
                   SELECT 1 FROM replicate_audit_holds o
                   WHERE o.stream_id = h.stream_id AND o.group_time = h.group_time
                     AND o.status IN ('pending', 'deferred')
                     AND (o.created_at, o.id) > (h.created_at, h.id)
               )",
        )
        .await?;
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
}
