use sea_orm_migration::prelude::*;

/// Say where every reading came from, on the row.
///
/// Q49 settled that only the origins nothing else records carry a stored blob (a tool run, a
/// chain, a CSV import, a hand entry, a batch); a sync or derived reading's story is resolved from
/// the stream, the receipt covering the instant and the definition it already names. What every
/// row carries either way is the discriminator, so an unrecorded origin is a named kind and never
/// a NULL blob.
///
/// Totality is held by the database, the way `readings_inherit_stream_instrument` holds
/// attribution: a writer that knows its origin declares it, and a writer that does not gets the
/// kind its own evidence proves. The backfill applies the same rule to the rows that predate the
/// column, stamping `migration` where nothing proves anything better.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    ALTER TABLE public.readings ADD COLUMN IF NOT EXISTS provenance_kind text;

    CREATE OR REPLACE FUNCTION public.readings_default_provenance_kind() RETURNS trigger
        LANGUAGE plpgsql
        AS $$
            DECLARE origin text;
            BEGIN
                IF NEW.provenance_kind IS NOT NULL THEN
                    RETURN NEW;
                END IF;
                IF NEW.measurement_type = 'derived' THEN
                    NEW.provenance_kind := 'derived';
                    RETURN NEW;
                END IF;
                SELECT source_system INTO origin FROM data_streams WHERE id = NEW.stream_id;
                NEW.provenance_kind := CASE
                    WHEN origin = 'grab_sample' THEN 'manual'
                    WHEN origin = 'api' THEN 'batch'
                    WHEN origin IS NOT NULL THEN 'sync'
                    ELSE 'migration'
                END;
                RETURN NEW;
            END;
            $$;

    DROP TRIGGER IF EXISTS trg_readings_default_provenance_kind ON public.readings;
    CREATE TRIGGER trg_readings_default_provenance_kind
        BEFORE INSERT ON public.readings
        FOR EACH ROW EXECUTE FUNCTION public.readings_default_provenance_kind();

    SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;

    UPDATE readings r
       SET provenance_kind = COALESCE(r.provenance ->> 'source', 'tool_run')
     WHERE r.provenance_kind IS NULL
       AND r.provenance IS NOT NULL;

    UPDATE readings r
       SET provenance_kind = CASE
               WHEN r.measurement_type = 'derived' THEN 'derived'
               WHEN ds.source_system = 'grab_sample' THEN 'manual'
               WHEN ds.source_system = 'api' THEN 'batch'
               WHEN ds.source_system IS NOT NULL THEN 'sync'
               ELSE 'migration'
           END
      FROM data_streams ds
     WHERE ds.id = r.stream_id
       AND r.provenance_kind IS NULL;

    UPDATE readings SET provenance_kind = 'migration' WHERE provenance_kind IS NULL;

    ALTER TABLE public.readings DROP CONSTRAINT IF EXISTS readings_provenance_kind_check;
    ALTER TABLE public.readings ADD CONSTRAINT readings_provenance_kind_check
        CHECK (provenance_kind IN ('tool_run', 'chain', 'csv_import', 'manual', 'batch', 'sync',
                                   'derived', 'migration'));
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TRIGGER IF EXISTS trg_readings_default_provenance_kind ON public.readings;
                 DROP FUNCTION IF EXISTS public.readings_default_provenance_kind();
                 ALTER TABLE public.readings DROP CONSTRAINT IF EXISTS readings_provenance_kind_check;
                 ALTER TABLE public.readings DROP COLUMN IF EXISTS provenance_kind;",
            )
            .await?;
        Ok(())
    }
}
