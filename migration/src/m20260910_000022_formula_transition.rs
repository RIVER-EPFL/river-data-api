use sea_orm_migration::prelude::*;

/// A recompute that moves a stored derived value records the move (Q116, M135).
///
/// The kind projects no column: the recompute writes the value itself, and the decision is the
/// record of the move, so the projection trigger's chain falls through it the way it does for
/// `chain`, `detach` and `return`. Only the CHECK has to learn the name.
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
            'formula_transition'::text, 'rollback'::text]));
";

pub const DOWN: &str = r"
    DELETE FROM public.reading_decisions WHERE kind = 'formula_transition';
    ALTER TABLE public.reading_decisions
        DROP CONSTRAINT IF EXISTS reading_decisions_kind_check;
    ALTER TABLE public.reading_decisions
        ADD CONSTRAINT reading_decisions_kind_check CHECK (kind = ANY (ARRAY[
            'flag'::text, 'unflag'::text, 'withdraw'::text, 'reassert'::text, 'curve'::text,
            'calibration_pin'::text, 'instrument_pin'::text, 'slot_move'::text,
            'value_correction'::text, 'unverified_entry'::text, 'verify'::text, 'reject'::text,
            'chain'::text, 'detach'::text, 'return'::text, 'curve_retire'::text,
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
