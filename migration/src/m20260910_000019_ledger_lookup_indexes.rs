use sea_orm_migration::prelude::*;

/// The two lookups the value ledger makes that no index answers.
///
/// `tool_runs` is indexed on `created_at` alone, so finding the runs made at one visit is a
/// sequential scan of the table; the ledger reaches them by `context->>'collection_event_id'`.
/// `ingest_receipts` is indexed by `(stream_id, at DESC)`, which answers "the stream's latest
/// passes" and not "the passes whose window covers this instant", so that arm scans a stream's
/// whole ledger.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
    CREATE INDEX IF NOT EXISTS idx_tool_runs_collection_event
        ON public.tool_runs ((context->>'collection_event_id'), created_at DESC)
        WHERE context ? 'collection_event_id';

    CREATE INDEX IF NOT EXISTS idx_ingest_receipts_window
        ON public.ingest_receipts (stream_id, window_from, window_to);
";

const DOWN: &str = "
    DROP INDEX IF EXISTS public.idx_tool_runs_collection_event;
    DROP INDEX IF EXISTS public.idx_ingest_receipts_window;
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
