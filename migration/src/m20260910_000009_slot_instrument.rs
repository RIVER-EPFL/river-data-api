use sea_orm_migration::prelude::*;

/// The instrument that measures a slot, declared rather than implied.
///
/// A hand-entered or calculated reading needs an instrument, and until now it took whichever one
/// the operator's chosen curve belonged to, or a bookkeeping instrument minted for the entry
/// channel. Neither is a statement about what measured the value. The slot names it here; the
/// entry channel's own instrument stays as the marker for a slot nobody has declared.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    ALTER TABLE public.site_parameters
        ADD COLUMN IF NOT EXISTS instrument_sensor_id uuid REFERENCES public.sensors(id);

    CREATE INDEX IF NOT EXISTS idx_site_parameters_instrument
        ON public.site_parameters (instrument_sensor_id)
        WHERE instrument_sensor_id IS NOT NULL;
";

pub const DOWN: &str = "
    DROP INDEX IF EXISTS public.idx_site_parameters_instrument;
    ALTER TABLE public.site_parameters DROP COLUMN IF EXISTS instrument_sensor_id;
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
