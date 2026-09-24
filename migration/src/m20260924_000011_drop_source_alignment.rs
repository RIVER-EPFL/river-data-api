use sea_orm_migration::prelude::*;

/// A source is read at the instant being computed and nowhere else (Q252): nothing holds a visit's
/// value across the pulses after it.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
ALTER TABLE public.derived_parameter_sources
    DROP CONSTRAINT IF EXISTS derived_parameter_sources_alignment_check,
    DROP COLUMN IF EXISTS alignment;
";

const DOWN: &str = r"
ALTER TABLE public.derived_parameter_sources
    ADD COLUMN alignment text NOT NULL DEFAULT 'exact',
    ADD CONSTRAINT derived_parameter_sources_alignment_check
        CHECK (alignment = ANY (ARRAY['exact'::text, 'hold'::text]));
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
