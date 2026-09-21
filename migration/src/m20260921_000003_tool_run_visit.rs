use sea_orm_migration::prelude::*;

/// The visit a tool run was computed at, as columns rather than as keys of its `context` blob.
///
/// `store_run` resolves the site and the instant before it writes the row and puts them in
/// `context`, where nothing can filter on them: `idx_tool_runs_collection_event` indexed a
/// `collection_event_id` key no writer has ever set, so "the runs at this visit" had no index and
/// no query. The columns carry the same two values the blob does, written together from the same
/// resolution, and the blob keeps its copy because it is the provenance snapshot a stored run is
/// replayed from. Existing rows are backfilled from that blob, so a run stored before this
/// migration is found at its visit like any other.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.tool_runs
                     ADD COLUMN IF NOT EXISTS site_id uuid REFERENCES public.sites(id) ON DELETE SET NULL,
                     ADD COLUMN IF NOT EXISTS collected_at timestamp with time zone;

                 UPDATE public.tool_runs
                    SET site_id = (context ->> 'site_id')::uuid
                  WHERE site_id IS NULL
                    AND context ->> 'site_id' IS NOT NULL
                    AND EXISTS (SELECT 1 FROM public.sites s WHERE s.id = (context ->> 'site_id')::uuid);

                 UPDATE public.tool_runs
                    SET collected_at = (context ->> 'collected_at')::timestamptz
                  WHERE collected_at IS NULL
                    AND context ->> 'collected_at' IS NOT NULL;

                 DROP INDEX IF EXISTS public.idx_tool_runs_collection_event;

                 CREATE INDEX IF NOT EXISTS idx_tool_runs_visit
                     ON public.tool_runs (site_id, collected_at, created_at DESC)
                  WHERE site_id IS NOT NULL;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP INDEX IF EXISTS public.idx_tool_runs_visit;
                 ALTER TABLE public.tool_runs
                     DROP COLUMN IF EXISTS site_id,
                     DROP COLUMN IF EXISTS collected_at;
                 CREATE INDEX IF NOT EXISTS idx_tool_runs_collection_event
                     ON public.tool_runs (((context ->> 'collection_event_id')), created_at DESC)
                  WHERE context ? 'collection_event_id';",
            )
            .await?;
        Ok(())
    }
}
