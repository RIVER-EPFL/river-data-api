use sea_orm_migration::prelude::*;

/// The version a computed value names, indexed, so a calculation's version ledger is an index
/// range rather than a scan of the curation ledger.
///
/// Only the two kinds that carry a computation are indexed: a flag or a withdrawal on a computed
/// reading is curation, and neither names a version. That keeps the index to the rows the ledger
/// groups by on a table every curation writer appends to.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_reading_decisions_derived_version
                     ON public.reading_decisions (((new ->> 'derived_version_id')))
                  WHERE kind IN ('derived_computed', 'formula_transition')",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP INDEX IF EXISTS public.idx_reading_decisions_derived_version",
            )
            .await?;
        Ok(())
    }
}
