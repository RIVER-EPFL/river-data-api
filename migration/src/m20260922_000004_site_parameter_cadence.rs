use sea_orm_migration::prelude::*;

/// The cadence a site fills a slot at, declared on the slot rather than inferred at the write.
///
/// `readings` holds one row per slot instant, so a slot computed both at a visit and on its
/// stream has the two engines overwriting each other's value and provenance in place. The
/// declaration is what splits them: `low` is the chain's at a visit, `high` is the stream
/// engine's. Existing rows take `high`, which is what the site parameters endpoint already falls
/// back to for a slot with no data.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.site_parameters
                     ADD COLUMN IF NOT EXISTS cadence text NOT NULL DEFAULT 'high';

                 ALTER TABLE public.site_parameters
                     DROP CONSTRAINT IF EXISTS site_parameters_cadence_check;

                 ALTER TABLE public.site_parameters
                     ADD CONSTRAINT site_parameters_cadence_check
                     CHECK (cadence = ANY (ARRAY['high'::text, 'low'::text]));",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.site_parameters
                     DROP CONSTRAINT IF EXISTS site_parameters_cadence_check;

                 ALTER TABLE public.site_parameters
                     DROP COLUMN IF EXISTS cadence",
            )
            .await?;
        Ok(())
    }
}
