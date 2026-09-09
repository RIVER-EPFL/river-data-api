use sea_orm_migration::prelude::*;

/// Declare which MeteoSwiss SMN station supplies a site's barometric pressure.
///
/// The oxygen-saturation procedure the public API publishes to Mount Resilience partners takes a
/// pressure time series matched to the water temperature stamps, and nothing in the tree carried
/// one. The station is a property of the site, so the mapping is data an operator fills in rather
/// than a table the code knows: `meteoswiss_sync` reads this column and does nothing for a site
/// that leaves it null.
///
/// The catalog parameter is not seeded with it: a database starts blank and a parameter is
/// something somebody at this lab agreed to (Q134). `meteoswiss_sync` says so and does nothing
/// until one exists, so a site naming a station before the parameter is a log line, not a loss.
/// Its units are the hectopascals `prestas0` reports; the procedure's pascals are a factor the
/// formula carries.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = "ALTER TABLE public.sites ADD COLUMN IF NOT EXISTS meteoswiss_station_abbr text;";

const DOWN: &str = "ALTER TABLE public.sites DROP COLUMN IF EXISTS meteoswiss_station_abbr;";

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
