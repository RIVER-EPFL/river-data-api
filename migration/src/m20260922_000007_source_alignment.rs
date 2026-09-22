use sea_orm_migration::prelude::*;

/// How a derived source reaches a reading (Q230): `exact` at the instant being computed, `hold`
/// for the last value measured at or before it.
///
/// A calculation on a high-frequency stream may read a value the lab measures at a visit. Read
/// exactly, that input exists at one instant a fortnight and the output exists nowhere else; held,
/// it stands at every pulse until the next visit measures a new one. The rule is per source
/// because one calculation reads both kinds: CO2 from the stream exactly, alkalinity from the
/// visit held. Existing rows take `exact`, which is what the binder has always done.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.derived_parameter_sources
                     ADD COLUMN IF NOT EXISTS alignment text NOT NULL DEFAULT 'exact';

                 ALTER TABLE public.derived_parameter_sources
                     DROP CONSTRAINT IF EXISTS derived_parameter_sources_alignment_check;

                 ALTER TABLE public.derived_parameter_sources
                     ADD CONSTRAINT derived_parameter_sources_alignment_check
                     CHECK (alignment = ANY (ARRAY['exact'::text, 'hold'::text]));",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.derived_parameter_sources
                     DROP CONSTRAINT IF EXISTS derived_parameter_sources_alignment_check;

                 ALTER TABLE public.derived_parameter_sources
                     DROP COLUMN IF EXISTS alignment",
            )
            .await?;
        Ok(())
    }
}
