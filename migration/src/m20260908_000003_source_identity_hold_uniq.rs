use sea_orm_migration::prelude::*;

/// One standing identity hold per channel.
///
/// A device-identity change is raised by every registration that reports it, and two overlapping
/// registrations (a discovery cycle against a triggered full sync, or two replicas) neither see
/// each other's uncommitted row nor collide on `replicate_audit_holds_live_uniq`, which is keyed on
/// `group_time`. The index below is the conflict target `raise_source_identity_hold` upserts on, so
/// the second pass updates the standing hold instead of opening a second one.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = "
    DELETE FROM public.replicate_audit_holds h
     WHERE h.kind = 'source_identity_changed'
       AND h.status IN ('pending', 'deferred')
       AND EXISTS (
         SELECT 1 FROM public.replicate_audit_holds o
          WHERE o.kind = 'source_identity_changed'
            AND o.status IN ('pending', 'deferred')
            AND o.stream_id = h.stream_id
            AND (o.created_at, o.id) > (h.created_at, h.id)
       );

    CREATE UNIQUE INDEX IF NOT EXISTS replicate_audit_holds_identity_live_uniq
        ON public.replicate_audit_holds (stream_id)
     WHERE kind = 'source_identity_changed' AND status IN ('pending', 'deferred');
";

const DOWN: &str = "
    DROP INDEX IF EXISTS public.replicate_audit_holds_identity_live_uniq;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }
}
