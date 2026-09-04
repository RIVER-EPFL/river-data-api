use sea_orm_migration::prelude::*;

/// Declare which MeteoSwiss SMN station supplies a site's barometric pressure.
///
/// The oxygen-saturation procedure the public API publishes to Mount Resilience partners takes a
/// pressure time series matched to the water temperature stamps, and nothing in the tree carried
/// one. The station is a property of the site, so the mapping is data an operator fills in rather
/// than a table the code knows: `meteoswiss_sync` reads this column and does nothing for a site
/// that leaves it null.
///
/// The catalog parameter is seeded here so the first sync has somewhere to land. Units are the
/// hectopascals `prestas0` reports; the procedure's pascals are a factor the formula carries.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = "
    ALTER TABLE public.sites ADD COLUMN IF NOT EXISTS meteoswiss_station_abbr text;

    INSERT INTO public.parameters (code, name, default_units, category, description)
    SELECT 'barometric_pressure', 'Barometric Pressure', 'hPa', 'measurement',
           'Station-level barometric pressure, MeteoSwiss SMN variable prestas0.'
     WHERE NOT EXISTS (
       SELECT 1 FROM public.parameters WHERE LOWER(code) = 'barometric_pressure'
     );
";

const DOWN: &str = "
    ALTER TABLE public.sites DROP COLUMN IF EXISTS meteoswiss_station_abbr;

    DELETE FROM public.parameters p
     WHERE LOWER(p.code) = 'barometric_pressure'
       AND NOT EXISTS (SELECT 1 FROM public.site_parameters sp WHERE sp.parameter_id = p.id);
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
