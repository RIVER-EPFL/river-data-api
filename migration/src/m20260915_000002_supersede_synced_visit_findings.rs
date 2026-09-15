use sea_orm_migration::prelude::*;

/// Close the event findings standing against synced visits.
///
/// The audit no longer covers a `portal_sync` visit, because the repair refuses one (Q41, Q175).
/// The findings raised while it did are holds no recompute can close, so they are superseded here.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "UPDATE public.replicate_audit_holds h
                    SET status = 'superseded'
                  WHERE h.status = 'pending'
                    AND h.stream_id IS NULL
                    AND h.kind IN ('missing_output', 'stale_output', 'skipped_output')
                    AND EXISTS (SELECT 1 FROM public.collection_events ce
                                 WHERE ce.site_id = h.site_id
                                   AND ce.collected_at = h.group_time
                                   AND ce.source = 'portal_sync')",
            )
            .await?;
        Ok(())
    }

    /// A superseded finding is not distinguishable from one superseded by a recompute, so nothing
    /// is reopened.
    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
