use sea_orm_migration::prelude::*;

/// Samples entity for user-declared replicate grouping.
///
/// Adds a `samples` table with metadata + aggregate columns maintained by one
/// PL/pgSQL trigger. Each reading can reference a sample via `readings.sample_id`.
/// Replicates within a sample are the readings sharing its sample_id.
///
/// Aggregates (mean/stdev/n/min/max) are stored directly on the samples row and
/// kept fresh by a trigger that fires on INSERT/UPDATE/DELETE of readings with
/// non-null sample_id. Serialized per sample via pg_advisory_xact_lock.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS samples (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id UUID NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                collected_at TIMESTAMPTZ NOT NULL,
                label TEXT,
                notes TEXT,
                field_trip_id UUID REFERENCES field_trips(id),
                created_by TEXT,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                mean DOUBLE PRECISION,
                stdev DOUBLE PRECISION,
                n INTEGER NOT NULL DEFAULT 0,
                min_value DOUBLE PRECISION,
                max_value DOUBLE PRECISION,
                updated_at TIMESTAMPTZ
            );
            CREATE INDEX IF NOT EXISTS samples_site_idx
                ON samples (site_id, collected_at DESC);
            CREATE INDEX IF NOT EXISTS samples_parameter_idx
                ON samples (parameter_id, collected_at DESC);
            CREATE INDEX IF NOT EXISTS samples_field_trip_idx
                ON samples (field_trip_id) WHERE field_trip_id IS NOT NULL;
            "#,
        )
        .await?;

        // TimescaleDB rejects ALTER ... ADD COLUMN with constraints on a hypertable
        // that has compression enabled. Remove the policy, decompress existing chunks,
        // turn off compression for the DDL, then restore the same compression config
        // and policy the init migration set up.
        db.execute_unprepared("SELECT remove_compression_policy('readings', if_exists => true)")
            .await
            .ok();

        db.execute_unprepared(
            r#"
            DO $$
            DECLARE
                chunk regclass;
            BEGIN
                FOR chunk IN
                    SELECT format('%I.%I', chunk_schema, chunk_name)::regclass
                    FROM timescaledb_information.chunks
                    WHERE hypertable_name = 'readings' AND is_compressed
                LOOP
                    EXECUTE format('SELECT decompress_chunk(%L)', chunk);
                END LOOP;
            END $$;
            "#,
        )
        .await
        .ok();

        db.execute_unprepared("ALTER TABLE readings SET (timescaledb.compress = false)")
            .await
            .ok();

        db.execute_unprepared(
            r#"
            ALTER TABLE readings
                ADD COLUMN IF NOT EXISTS sample_id UUID NULL
                    REFERENCES samples(id) ON DELETE SET NULL;
            CREATE INDEX IF NOT EXISTS idx_readings_sample_id
                ON readings (sample_id)
                WHERE sample_id IS NOT NULL;
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            ALTER TABLE readings SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'stream_id'
            )
            "#,
        )
        .await
        .ok();

        db.execute_unprepared("SELECT add_compression_policy('readings', INTERVAL '30 days')")
            .await
            .ok();

        db.execute_unprepared(
            r#"
            CREATE OR REPLACE FUNCTION refresh_sample_aggregate(target_sample_id UUID)
            RETURNS void AS $$
            BEGIN
                IF target_sample_id IS NULL THEN
                    RETURN;
                END IF;

                -- Serialize concurrent refreshes for the same sample.
                PERFORM pg_advisory_xact_lock(
                    hashtextextended(target_sample_id::text, 0)
                );

                UPDATE samples s
                SET mean       = a.mean,
                    stdev      = a.stdev,
                    n          = COALESCE(a.n, 0),
                    min_value  = a.min_value,
                    max_value  = a.max_value,
                    updated_at = NOW()
                FROM (
                    SELECT
                        AVG(COALESCE(calibrated_value, raw_value))         AS mean,
                        STDDEV_SAMP(COALESCE(calibrated_value, raw_value)) AS stdev,
                        COUNT(*)::INTEGER                                   AS n,
                        MIN(COALESCE(calibrated_value, raw_value))         AS min_value,
                        MAX(COALESCE(calibrated_value, raw_value))         AS max_value
                    FROM readings
                    WHERE sample_id = target_sample_id
                      AND is_flagged = FALSE
                ) a
                WHERE s.id = target_sample_id;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE OR REPLACE FUNCTION samples_on_reading_insert() RETURNS trigger AS $$
            BEGIN
                PERFORM refresh_sample_aggregate(NEW.sample_id);
                RETURN NULL;
            END;
            $$ LANGUAGE plpgsql;

            CREATE OR REPLACE FUNCTION samples_on_reading_delete() RETURNS trigger AS $$
            BEGIN
                PERFORM refresh_sample_aggregate(OLD.sample_id);
                RETURN NULL;
            END;
            $$ LANGUAGE plpgsql;

            CREATE OR REPLACE FUNCTION samples_on_reading_update() RETURNS trigger AS $$
            BEGIN
                PERFORM refresh_sample_aggregate(OLD.sample_id);
                IF NEW.sample_id IS DISTINCT FROM OLD.sample_id THEN
                    PERFORM refresh_sample_aggregate(NEW.sample_id);
                END IF;
                RETURN NULL;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            DROP TRIGGER IF EXISTS trg_readings_sample_refresh_ins ON readings;
            CREATE TRIGGER trg_readings_sample_refresh_ins
                AFTER INSERT ON readings
                FOR EACH ROW
                WHEN (NEW.sample_id IS NOT NULL)
                EXECUTE FUNCTION samples_on_reading_insert();

            DROP TRIGGER IF EXISTS trg_readings_sample_refresh_del ON readings;
            CREATE TRIGGER trg_readings_sample_refresh_del
                AFTER DELETE ON readings
                FOR EACH ROW
                WHEN (OLD.sample_id IS NOT NULL)
                EXECUTE FUNCTION samples_on_reading_delete();

            DROP TRIGGER IF EXISTS trg_readings_sample_refresh_upd ON readings;
            CREATE TRIGGER trg_readings_sample_refresh_upd
                AFTER UPDATE ON readings
                FOR EACH ROW
                WHEN (
                       OLD.sample_id       IS DISTINCT FROM NEW.sample_id
                    OR OLD.raw_value        IS DISTINCT FROM NEW.raw_value
                    OR OLD.calibrated_value IS DISTINCT FROM NEW.calibrated_value
                    OR OLD.is_flagged       IS DISTINCT FROM NEW.is_flagged
                )
                EXECUTE FUNCTION samples_on_reading_update();
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            DROP TRIGGER IF EXISTS trg_readings_sample_refresh_ins ON readings;
            DROP TRIGGER IF EXISTS trg_readings_sample_refresh_del ON readings;
            DROP TRIGGER IF EXISTS trg_readings_sample_refresh_upd ON readings;
            DROP FUNCTION IF EXISTS samples_on_reading_insert();
            DROP FUNCTION IF EXISTS samples_on_reading_delete();
            DROP FUNCTION IF EXISTS samples_on_reading_update();
            DROP FUNCTION IF EXISTS refresh_sample_aggregate(UUID);
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            DROP INDEX IF EXISTS idx_readings_sample_id;
            ALTER TABLE readings DROP COLUMN IF EXISTS sample_id;
            "#,
        )
        .await?;

        db.execute_unprepared("DROP TABLE IF EXISTS samples CASCADE")
            .await?;

        Ok(())
    }
}
