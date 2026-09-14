use sea_orm_migration::prelude::*;

/// The SMN station list, maintained from `ogd-smn_meta_stations.csv`.
///
/// Nothing held the stations, so an abbreviation was whatever an operator typed and a typo
/// subscribed a site to a URL that 404s.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS public.meteoswiss_stations (
                     station_abbr text NOT NULL PRIMARY KEY,
                     name text NOT NULL,
                     data_since date,
                     height_masl double precision,
                     height_barometer_masl double precision,
                     latitude double precision,
                     longitude double precision,
                     updated_at timestamp with time zone DEFAULT now() NOT NULL
                 )",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS public.meteoswiss_stations")
            .await?;
        Ok(())
    }
}
