use sea_orm_migration::prelude::*;

/// A reading on an unpaired stream is staged: it names no site, no parameter and no instrument.
///
/// The instrument was the one attribution such a row still carried, because
/// `readings_instrument_required` refused a row without one and
/// `readings_inherit_stream_instrument` filled it from the stream, which registration had given an
/// instrument nobody chose. So 362k readings on dev were stored as measurements of an instrument no
/// plan had settled on. Q134's rule is that they are staged until the plan validates them, and the
/// pairing backfill is what stamps all three together.
///
/// The check keeps its meaning for everything that is attributed: an unattributed row is by
/// definition unpaired, so `site_id IS NULL` is the exemption rather than a hole. The trigger asks
/// the same question of the row it is filling: an attributed reading naming no instrument takes its
/// stream's, and a staged one is left alone.
///
/// The trigger is replaced before the sweep, not after: it fires on UPDATE as well as INSERT, so
/// the old one fills the column straight back in. The UPDATE runs with
/// `max_tuples_decompressed_per_dml_transaction = 0` so it cannot fail on a compressed chunk, the
/// way every other bulk readings write here does.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r"
    ALTER TABLE public.readings DROP CONSTRAINT IF EXISTS readings_instrument_required;
    ALTER TABLE public.readings ADD CONSTRAINT readings_instrument_required
        CHECK (sensor_id IS NOT NULL OR site_id IS NULL
               OR measurement_type IS NOT DISTINCT FROM 'derived');

    CREATE OR REPLACE FUNCTION public.readings_inherit_stream_instrument() RETURNS trigger
        LANGUAGE plpgsql
        AS $$
            BEGIN
                IF NEW.sensor_id IS NULL AND NEW.site_id IS NOT NULL
                   AND NEW.measurement_type IS DISTINCT FROM 'derived' THEN
                    SELECT sensor_id INTO NEW.sensor_id FROM data_streams WHERE id = NEW.stream_id;
                END IF;
                RETURN NEW;
            END;
            $$;

    SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;

    UPDATE public.readings r
       SET sensor_id = NULL, calibration_id = NULL, deployment_id = NULL
      FROM public.data_streams s
     WHERE s.id = r.stream_id
       AND s.site_parameter_id IS NULL
       AND r.site_id IS NULL
       AND r.measurement_type IS DISTINCT FROM 'derived'
       AND (r.sensor_id IS NOT NULL OR r.calibration_id IS NOT NULL
            OR r.deployment_id IS NOT NULL);

";

const DOWN: &str = r"
    ALTER TABLE public.readings DROP CONSTRAINT IF EXISTS readings_instrument_required;

    CREATE OR REPLACE FUNCTION public.readings_inherit_stream_instrument() RETURNS trigger
        LANGUAGE plpgsql
        AS $$
            BEGIN
                IF NEW.sensor_id IS NULL AND NEW.measurement_type IS DISTINCT FROM 'derived' THEN
                    SELECT sensor_id INTO NEW.sensor_id FROM data_streams WHERE id = NEW.stream_id;
                END IF;
                RETURN NEW;
            END;
            $$;

    SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;

    UPDATE public.readings r
       SET sensor_id = s.sensor_id
      FROM public.data_streams s
     WHERE s.id = r.stream_id
       AND r.sensor_id IS NULL
       AND s.sensor_id IS NOT NULL
       AND r.measurement_type IS DISTINCT FROM 'derived';

    ALTER TABLE public.readings ADD CONSTRAINT readings_instrument_required
        CHECK (sensor_id IS NOT NULL OR measurement_type IS NOT DISTINCT FROM 'derived');
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
