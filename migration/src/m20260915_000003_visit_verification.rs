use sea_orm_migration::prelude::*;

/// A visit carries its own verification state, so a field day can be ruled on before its values
/// are (Q177).
///
/// `unverified` is stamped at staging from the stager's level, exactly as `readings.unverified` is
/// stamped from the writer's. `withdrawn_at` is what a rejection leaves behind: nothing deletes a
/// visit, and its readings are withdrawn beside it. The hold subject CHECK widens to let an
/// `unverified_visit` finding name a site and an instant without a parameter, because a visit is
/// not a slot, and the open-unique index beside it makes a second staging refresh the one hold.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE public.collection_events
                 ADD COLUMN IF NOT EXISTS unverified boolean DEFAULT false NOT NULL,
                 ADD COLUMN IF NOT EXISTS withdrawn_at timestamp with time zone",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.replicate_audit_holds
                 DROP CONSTRAINT IF EXISTS audit_hold_subject,
                 ADD CONSTRAINT audit_hold_subject CHECK (
                     stream_id IS NOT NULL
                     OR (kind = 'unverified_visit' AND site_id IS NOT NULL)
                     OR (kind <> 'replicate_stats' AND site_id IS NOT NULL
                         AND parameter_id IS NOT NULL))",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS replicate_audit_holds_visit_live_uniq
                 ON public.replicate_audit_holds (kind, site_id, group_time)
               WHERE stream_id IS NULL AND parameter_id IS NULL AND status = 'pending'",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "DROP INDEX IF EXISTS public.replicate_audit_holds_visit_live_uniq",
        )
        .await?;
        db.execute_unprepared(
            "DELETE FROM public.replicate_audit_holds WHERE kind = 'unverified_visit'",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.replicate_audit_holds
                 DROP CONSTRAINT IF EXISTS audit_hold_subject,
                 ADD CONSTRAINT audit_hold_subject CHECK (
                     stream_id IS NOT NULL
                     OR (kind <> 'replicate_stats' AND site_id IS NOT NULL
                         AND parameter_id IS NOT NULL))",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.collection_events
                 DROP COLUMN IF EXISTS unverified,
                 DROP COLUMN IF EXISTS withdrawn_at",
        )
        .await?;
        Ok(())
    }
}
