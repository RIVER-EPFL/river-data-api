use sea_orm_migration::prelude::*;

/// Close the NULL hole in `readings_instrument_required`.
///
/// The rule was written as `sensor_id IS NOT NULL OR measurement_type = 'derived'`. A reading with
/// no instrument and no classification makes the second disjunct NULL, so the whole expression is
/// NULL and the CHECK admits the row: a measurement with no instrument, which is what the rule
/// exists to refuse. An absent `measurement_type` reads as continuous everywhere else, so it claims
/// no exemption here either.
///
/// Every database reaching this point has had its readings attributed by
/// `m20260906_000001_attribute_existing_readings`, so no stored row is refused by the tightening.
#[derive(DeriveMigrationName)]
pub struct Migration;

const TIGHTEN: &str = "
    ALTER TABLE public.readings DROP CONSTRAINT IF EXISTS readings_instrument_required;
    ALTER TABLE public.readings ADD CONSTRAINT readings_instrument_required
        CHECK ((sensor_id IS NOT NULL) OR (measurement_type IS NOT DISTINCT FROM 'derived'));
";

const LOOSEN: &str = "
    ALTER TABLE public.readings DROP CONSTRAINT IF EXISTS readings_instrument_required;
    ALTER TABLE public.readings ADD CONSTRAINT readings_instrument_required
        CHECK ((sensor_id IS NOT NULL) OR (measurement_type = 'derived'));
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(TIGHTEN).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(LOOSEN).await?;
        Ok(())
    }
}
