use sea_orm_migration::prelude::*;

/// The ledger accepts `attribution`, the kind a pairing, an adopt or swap, or a deployment rollback
/// records for each reading it attributed.
#[derive(DeriveMigrationName)]
pub struct Migration;

const KINDS: &str = "'flag', 'unflag', 'withdraw', 'reassert', 'curve', 'calibration_pin', \
     'instrument_pin', 'slot_move', 'value_correction', 'unverified_entry', 'verify', 'reject', \
     'chain', 'detach', 'return', 'curve_retire', 'formula_transition', 'curve_recompose', \
     'derived_computed', 'reprocess', 'rollback', 'retag'";

fn check(extra: &str) -> String {
    format!(
        "ALTER TABLE public.reading_decisions DROP CONSTRAINT reading_decisions_kind_check;
         ALTER TABLE public.reading_decisions ADD CONSTRAINT reading_decisions_kind_check
             CHECK (kind = ANY (ARRAY[{KINDS}{extra}]))"
    )
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&check(", 'attribution'"))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&check(""))
            .await?;
        Ok(())
    }
}
