use sea_orm_migration::prelude::*;

/// Attribute the readings a database built before the instrument rule already holds, then add the
/// rule to it.
///
/// The baseline creates `readings_instrument_required` and `readings_inherit_stream_instrument`
/// with the table, so a fresh database is born under the rule and this migration finds nothing to
/// do. A database carried forward by the chain has neither, and holds rows the CHECK refuses:
/// readings on streams that never got an instrument, and derived readings written before the
/// compute path stamped `measurement_type`, which the exemption tests. Adding the constraint to
/// such a database aborts on the first of them, so the rows are made to satisfy it first.
///
/// It repairs the rows in place, so it must reach the carried-forward copy before that copy is
/// dumped: the target database is built from the baseline and restored into, and by then the CHECK
/// is already on the table. The step that gets it there is in the baseline's own doc comment.
///
/// The instrument identity is the one `sensors::operations::resolve_or_mint_stream_instrument`
/// applies, so a stream attributed here and a stream attributed by a later registration converge
/// on the same row: a device feed takes its own channel, anything else the source's instrument for
/// the parameter it carries.
#[derive(DeriveMigrationName)]
pub struct Migration;

/// Declare what a derived stream and its readings are.
///
/// The exemption reads the reading's own column, and a derived value written before that column
/// was stamped carries NULL, which reads as continuous. These rows have no instrument and never
/// will: a computed quantity is not an instrument measurement.
const DECLARE_DERIVED: &str = r"
    UPDATE data_streams SET measurement_type = 'derived'
     WHERE source_system = 'derived' AND measurement_type IS NULL;

    UPDATE readings r SET measurement_type = 'derived'
      FROM data_streams ds
     WHERE ds.id = r.stream_id
       AND ds.measurement_type = 'derived'
       AND r.measurement_type IS DISTINCT FROM 'derived';
";

/// Mint the instrument every remaining stream is missing, and link the stream to it.
///
/// Two passes over one identity so a key shared by several streams mints one row: the distinct
/// keys are inserted, then every stream is linked by looking its key up. A bookkeeping instrument
/// is minted `high` for the same reason `resolve_or_mint_stream_instrument` does: `data_frequency`
/// is read as a cadence declaration, and this instrument carries no evidence about cadence.
const MINT_STREAM_INSTRUMENTS: &str = r"
    CREATE TEMPORARY TABLE stream_instrument_identity ON COMMIT DROP AS
    SELECT ds.id AS stream_id,
           device.is_device AS is_device,
           CASE WHEN device.is_device THEN ds.source_key
                ELSE ds.source_system || ':' || COALESCE(
                    NULLIF(ds.metadata #>> '{hierarchy,parameter}', ''), ds.source_key)
           END AS source_key,
           CASE WHEN slot.site_name IS NOT NULL AND slot.parameter_name IS NOT NULL
                     AND ds.source_system IN ('api', 'grab_sample')
                THEN slot.site_name || ' ' || slot.parameter_name || ' (' || ds.source_system || ')'
                WHEN slot.site_name IS NOT NULL AND slot.parameter_name IS NOT NULL
                THEN slot.site_name || ' ' || slot.parameter_name
                WHEN device.is_device
                THEN COALESCE(ds.source_name, 'Stream ' || ds.source_key)
                ELSE COALESCE(NULLIF(ds.metadata #>> '{hierarchy,parameter}', ''), ds.source_key)
                     || ' (' || ds.source_system || ')'
           END AS name,
           ds.source_system
      FROM data_streams ds
      LEFT JOIN LATERAL (
          SELECT (ds.metadata -> 'device') IS NOT NULL
             AND jsonb_typeof(ds.metadata -> 'device') <> 'null' AS is_device
      ) device ON true
      LEFT JOIN LATERAL (
          SELECT s.name AS site_name, p.name AS parameter_name
            FROM site_parameters sp
            JOIN sites s ON s.id = sp.site_id
            JOIN parameters p ON p.id = sp.parameter_id
           WHERE sp.id = ds.site_parameter_id
      ) slot ON true
     WHERE ds.sensor_id IS NULL
       AND ds.measurement_type IS DISTINCT FROM 'derived';

    INSERT INTO sensors (name, is_lab_instrument, data_frequency, source_system, source_key, metadata)
    SELECT DISTINCT ON (i.source_system, i.source_key)
           i.name,
           NOT i.is_device,
           'high',
           i.source_system,
           i.source_key,
           '{}'::jsonb
      FROM stream_instrument_identity i
     ORDER BY i.source_system, i.source_key, i.stream_id
        ON CONFLICT (source_system, source_key)
           WHERE source_system IS NOT NULL AND source_key IS NOT NULL
        DO NOTHING;

    UPDATE data_streams ds
       SET sensor_id = s.id
      FROM stream_instrument_identity i
      JOIN sensors s
        ON s.source_system = i.source_system AND s.source_key = i.source_key
     WHERE ds.id = i.stream_id;
";

/// Stamp the readings that inherit their stream's instrument.
///
/// `max_tuples_decompressed_per_dml_transaction = 0` lifts the per-transaction decompression cap,
/// without which the UPDATE fails on chunks older than the 30-day compression policy.
const STAMP_READINGS: &str = r"
    SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;

    UPDATE readings r
       SET sensor_id = ds.sensor_id
      FROM data_streams ds
     WHERE ds.id = r.stream_id
       AND r.sensor_id IS NULL
       AND ds.sensor_id IS NOT NULL
       AND r.measurement_type IS DISTINCT FROM 'derived';
";

/// Add the rule itself, for a database whose table was created without it.
const ADD_RULE: &str = r"
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

    DROP TRIGGER IF EXISTS trg_readings_inherit_stream_instrument ON public.readings;
    CREATE TRIGGER trg_readings_inherit_stream_instrument
        BEFORE INSERT OR UPDATE ON public.readings
        FOR EACH ROW EXECUTE FUNCTION public.readings_inherit_stream_instrument();

    ALTER TABLE public.readings DROP CONSTRAINT IF EXISTS readings_instrument_required;
    ALTER TABLE public.readings ADD CONSTRAINT readings_instrument_required
        CHECK ((sensor_id IS NOT NULL) OR (measurement_type = 'derived'));
";

/// The whole repair, in the order it must run: declare, mint, stamp, then constrain.
#[must_use]
pub fn attribute_existing_readings() -> String {
    format!("{DECLARE_DERIVED}{MINT_STREAM_INSTRUMENTS}{STAMP_READINGS}{ADD_RULE}")
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(&attribute_existing_readings()).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.readings DROP CONSTRAINT IF EXISTS readings_instrument_required;",
            )
            .await?;
        Ok(())
    }
}
