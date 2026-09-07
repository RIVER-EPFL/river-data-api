use sea_orm_migration::prelude::*;

/// A value the source changed after river-data stored it is proposed, not written (Q84).
///
/// One live proposal per reading key. The source re-asserts its whole window every cycle, so a
/// decision is recorded against the exact value that was decided on: a rejected proposal keeps its
/// `proposed_raw_value`, and the diff re-raises nothing while the source still asserts that number.
/// A different number at source replaces the row with a fresh `pending` proposal.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
    CREATE TABLE IF NOT EXISTS public.reading_change_proposals (
        id                        uuid PRIMARY KEY DEFAULT gen_random_uuid(),
        stream_id                 uuid NOT NULL REFERENCES public.data_streams(id) ON DELETE CASCADE,
        time                      timestamptz NOT NULL,
        replicate_index           smallint NOT NULL,
        proposed_raw_value        double precision NOT NULL,
        proposed_standard_curve_id uuid,
        stored_raw_value          double precision NOT NULL,
        stored_standard_curve_id  uuid,
        status                    text NOT NULL DEFAULT 'pending',
        first_seen_at             timestamptz NOT NULL DEFAULT now(),
        last_seen_at              timestamptz NOT NULL DEFAULT now(),
        decided_by                text,
        decided_at                timestamptz,
        CONSTRAINT reading_change_proposals_status_check
            CHECK (status = ANY (ARRAY['pending'::text, 'accepted'::text, 'rejected'::text])),
        CONSTRAINT reading_change_proposals_key UNIQUE (stream_id, time, replicate_index)
    );

    CREATE INDEX IF NOT EXISTS idx_reading_change_proposals_pending
        ON public.reading_change_proposals (stream_id, time) WHERE status = 'pending';

    ALTER TABLE public.ingest_receipts
        ADD COLUMN IF NOT EXISTS proposed integer NOT NULL DEFAULT 0;
";

const DOWN: &str = "
    ALTER TABLE public.ingest_receipts DROP COLUMN IF EXISTS proposed;
    DROP TABLE IF EXISTS public.reading_change_proposals;
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
