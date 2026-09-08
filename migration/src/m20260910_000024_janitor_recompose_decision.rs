use sea_orm_migration::prelude::*;

/// The janitor's curve-drift sweep records what it moved (Q118, M159).
///
/// The kind projects no column, because the sweep's own UPDATE writes the value; the decision is
/// the record of the move, so the projection trigger falls through it as it does for
/// `formula_transition`. `job_id` names the run that made the change and is cleared rather than
/// blocking when that job row is pruned.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r"
    ALTER TABLE public.reading_decisions
        ADD COLUMN IF NOT EXISTS job_id uuid
            REFERENCES public.reprocessing_jobs(id) ON DELETE SET NULL;
    ALTER TABLE public.reading_decisions
        DROP CONSTRAINT IF EXISTS reading_decisions_kind_check;
    ALTER TABLE public.reading_decisions
        ADD CONSTRAINT reading_decisions_kind_check CHECK (kind = ANY (ARRAY[
            'flag'::text, 'unflag'::text, 'withdraw'::text, 'reassert'::text, 'curve'::text,
            'calibration_pin'::text, 'instrument_pin'::text, 'slot_move'::text,
            'value_correction'::text, 'unverified_entry'::text, 'verify'::text, 'reject'::text,
            'chain'::text, 'detach'::text, 'return'::text, 'curve_retire'::text,
            'formula_transition'::text, 'curve_recompose'::text, 'rollback'::text]));
    ALTER TABLE public.reading_decisions
        DROP CONSTRAINT IF EXISTS reading_decisions_origin_check;
    ALTER TABLE public.reading_decisions
        ADD CONSTRAINT reading_decisions_origin_check CHECK (origin = ANY (ARRAY[
            'manual'::text, 'sync'::text, 'csv'::text, 'audit'::text, 'chain'::text,
            'rollback'::text, 'migration'::text, 'system'::text, 'janitor'::text]));
";

pub const DOWN: &str = r"
    DELETE FROM public.reading_decisions WHERE kind = 'curve_recompose' OR origin = 'janitor';
    ALTER TABLE public.reading_decisions DROP COLUMN IF EXISTS job_id;
    ALTER TABLE public.reading_decisions
        DROP CONSTRAINT IF EXISTS reading_decisions_kind_check;
    ALTER TABLE public.reading_decisions
        ADD CONSTRAINT reading_decisions_kind_check CHECK (kind = ANY (ARRAY[
            'flag'::text, 'unflag'::text, 'withdraw'::text, 'reassert'::text, 'curve'::text,
            'calibration_pin'::text, 'instrument_pin'::text, 'slot_move'::text,
            'value_correction'::text, 'unverified_entry'::text, 'verify'::text, 'reject'::text,
            'chain'::text, 'detach'::text, 'return'::text, 'curve_retire'::text,
            'formula_transition'::text, 'rollback'::text]));
    ALTER TABLE public.reading_decisions
        DROP CONSTRAINT IF EXISTS reading_decisions_origin_check;
    ALTER TABLE public.reading_decisions
        ADD CONSTRAINT reading_decisions_origin_check CHECK (origin = ANY (ARRAY[
            'manual'::text, 'sync'::text, 'csv'::text, 'audit'::text, 'chain'::text,
            'rollback'::text, 'migration'::text, 'system'::text]));
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
