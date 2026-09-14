use sea_orm_migration::prelude::*;

/// A site subscribes to a MeteoSwiss station and a variable per row, rather than through one text
/// column naming a station and standing for one hard-coded variable.
///
/// The column carried a station and nothing else, so a site could take station pressure or nothing,
/// and blanking it to stop the pressure also forgot the station.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS public.meteoswiss_subscriptions (
                 id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
                 site_id uuid NOT NULL REFERENCES public.sites(id) ON DELETE CASCADE,
                 station_abbr text NOT NULL,
                 variable text NOT NULL,
                 parameter_id uuid REFERENCES public.parameters(id) ON DELETE SET NULL,
                 enabled boolean DEFAULT true NOT NULL,
                 created_at timestamp with time zone DEFAULT now()
             );
             CREATE UNIQUE INDEX IF NOT EXISTS meteoswiss_subscriptions_slot_key
                 ON public.meteoswiss_subscriptions (site_id, upper(station_abbr), lower(variable));
             CREATE INDEX IF NOT EXISTS meteoswiss_subscriptions_site_id_idx
                 ON public.meteoswiss_subscriptions (site_id)",
        )
        .await?;
        // The station a site already declared becomes its pressure subscription, on the variable
        // the job read it as.
        db.execute_unprepared(
            "INSERT INTO public.meteoswiss_subscriptions (site_id, station_abbr, variable, parameter_id)
             SELECT s.id, upper(btrim(s.meteoswiss_station_abbr)), 'prestas0',
                    (SELECT p.id FROM public.parameters p WHERE lower(p.code) = 'barometric_pressure')
               FROM public.sites s
              WHERE btrim(coalesce(s.meteoswiss_station_abbr, '')) <> ''
             ON CONFLICT DO NOTHING",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.sites DROP COLUMN IF EXISTS meteoswiss_station_abbr",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE public.sites ADD COLUMN IF NOT EXISTS meteoswiss_station_abbr text",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE public.sites s
                SET meteoswiss_station_abbr = sub.station_abbr
               FROM public.meteoswiss_subscriptions sub
              WHERE sub.site_id = s.id AND sub.enabled",
        )
        .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS public.meteoswiss_subscriptions")
            .await?;
        Ok(())
    }
}
