use sea_orm_migration::prelude::*;

/// A reprocess records the readings it moved (Q118, Q125, M160).
///
/// Record only, as `formula_transition`, `curve_recompose` and `derived_computed` are: the bulk
/// statements write the attribution and the corrected value, and the decision says what each
/// column held before.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r"
    ALTER TABLE public.reading_decisions
        DROP CONSTRAINT IF EXISTS reading_decisions_kind_check;
    ALTER TABLE public.reading_decisions
        ADD CONSTRAINT reading_decisions_kind_check CHECK (kind = ANY (ARRAY[
            'flag'::text, 'unflag'::text, 'withdraw'::text, 'reassert'::text, 'curve'::text,
            'calibration_pin'::text, 'instrument_pin'::text, 'slot_move'::text,
            'value_correction'::text, 'unverified_entry'::text, 'verify'::text, 'reject'::text,
            'chain'::text, 'detach'::text, 'return'::text, 'curve_retire'::text,
            'formula_transition'::text, 'curve_recompose'::text, 'derived_computed'::text,
            'reprocess'::text, 'rollback'::text]));
";

pub const DOWN: &str = r"
    DELETE FROM public.reading_decisions WHERE kind = 'reprocess';
    ALTER TABLE public.reading_decisions
        DROP CONSTRAINT IF EXISTS reading_decisions_kind_check;
    ALTER TABLE public.reading_decisions
        ADD CONSTRAINT reading_decisions_kind_check CHECK (kind = ANY (ARRAY[
            'flag'::text, 'unflag'::text, 'withdraw'::text, 'reassert'::text, 'curve'::text,
            'calibration_pin'::text, 'instrument_pin'::text, 'slot_move'::text,
            'value_correction'::text, 'unverified_entry'::text, 'verify'::text, 'reject'::text,
            'chain'::text, 'detach'::text, 'return'::text, 'curve_retire'::text,
            'formula_transition'::text, 'curve_recompose'::text, 'derived_computed'::text,
            'rollback'::text]));
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
