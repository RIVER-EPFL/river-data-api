use sea_orm_migration::prelude::*;

/// Audit schema migration: alarm types, default thresholds, replicate index.
///
/// 1. alarm_thresholds: add alarm_type column + replace unique indexes
/// 2. parameters: add default threshold columns
/// 3. readings: add replicate_index, rebuild PK, recreate continuous aggregates
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // =================================================================
        // 1. alarm_thresholds: alarm_type + new unique indexes
        // =================================================================

        // Drop old partial unique indexes (they don't include alarm_type)
        db.execute_unprepared("DROP INDEX IF EXISTS idx_alarm_thresholds_param_site")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_alarm_thresholds_param_global")
            .await?;

        // Add alarm_type column
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds ADD COLUMN IF NOT EXISTS alarm_type VARCHAR(20) NOT NULL DEFAULT 'range'",
        )
        .await?;

        // New partial unique indexes including alarm_type
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_thresholds_param_site_type ON alarm_thresholds(parameter_id, site_id, alarm_type) WHERE site_id IS NOT NULL",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_thresholds_param_null_site_type ON alarm_thresholds(parameter_id, alarm_type) WHERE site_id IS NULL",
        )
        .await?;

        // =================================================================
        // 2. parameters: default threshold columns
        // =================================================================

        db.execute_unprepared(
            "ALTER TABLE parameters ADD COLUMN IF NOT EXISTS default_warning_min DOUBLE PRECISION",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE parameters ADD COLUMN IF NOT EXISTS default_warning_max DOUBLE PRECISION",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE parameters ADD COLUMN IF NOT EXISTS default_alarm_min DOUBLE PRECISION",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE parameters ADD COLUMN IF NOT EXISTS default_alarm_max DOUBLE PRECISION",
        )
        .await?;

        // =================================================================
        // 3. readings: add replicate_index, rebuild PK, recreate aggregates
        // =================================================================

        // -- 3a: Remove compression policy
        db.execute_unprepared("SELECT remove_compression_policy('readings', if_exists => true)")
            .await?;

        // -- 3b: Decompress all chunks
        db.execute_unprepared(
            r"DO $$
            DECLARE
                chunk REGCLASS;
            BEGIN
                FOR chunk IN
                    SELECT format('%I.%I', chunk_schema, chunk_name)::regclass
                    FROM timescaledb_information.chunks
                    WHERE hypertable_name = 'readings'
                      AND is_compressed = true
                LOOP
                    PERFORM decompress_chunk(chunk, if_compressed => true);
                END LOOP;
            END $$",
        )
        .await?;

        // -- 3c: Drop refresh policies
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_monthly', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_weekly', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_daily', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_hourly', if_exists => true)",
        )
        .await?;

        // -- 3d: Drop continuous aggregates
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_monthly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_weekly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_daily CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_hourly CASCADE")
            .await?;

        // -- 3e: Add replicate_index column
        db.execute_unprepared(
            "ALTER TABLE readings ADD COLUMN IF NOT EXISTS replicate_index SMALLINT NOT NULL DEFAULT 0",
        )
        .await?;

        // -- 3f: Rebuild primary key to include replicate_index
        // TimescaleDB hypertables may use a different PK constraint name, so find it dynamically
        db.execute_unprepared(
            r"DO $$
            DECLARE
                pk_name TEXT;
            BEGIN
                SELECT conname INTO pk_name
                FROM pg_constraint
                WHERE conrelid = 'readings'::regclass
                  AND contype = 'p';
                IF pk_name IS NOT NULL THEN
                    EXECUTE format('ALTER TABLE readings DROP CONSTRAINT %I', pk_name);
                END IF;
            END $$",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE readings ADD PRIMARY KEY (site_id, parameter_id, time, replicate_index)",
        )
        .await?;

        // -- 3g: Recreate continuous aggregates with replicate_index = 0 filter
        db.execute_unprepared(
            r"CREATE MATERIALIZED VIEW IF NOT EXISTS readings_hourly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 hour', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE measurement_type = 'continuous' AND is_flagged IS NOT TRUE AND replicate_index = 0
            GROUP BY time_bucket('1 hour', time), site_id, parameter_id
            WITH NO DATA",
        )
        .await?;

        db.execute_unprepared(
            r"CREATE MATERIALIZED VIEW IF NOT EXISTS readings_daily
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 day', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE measurement_type = 'continuous' AND is_flagged IS NOT TRUE AND replicate_index = 0
            GROUP BY time_bucket('1 day', time), site_id, parameter_id
            WITH NO DATA",
        )
        .await?;

        db.execute_unprepared(
            r"CREATE MATERIALIZED VIEW IF NOT EXISTS readings_weekly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 week', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE measurement_type = 'continuous' AND is_flagged IS NOT TRUE AND replicate_index = 0
            GROUP BY time_bucket('1 week', time), site_id, parameter_id
            WITH NO DATA",
        )
        .await?;

        db.execute_unprepared(
            r"CREATE MATERIALIZED VIEW IF NOT EXISTS readings_monthly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 month', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE measurement_type = 'continuous' AND is_flagged IS NOT TRUE AND replicate_index = 0
            GROUP BY time_bucket('1 month', time), site_id, parameter_id
            WITH NO DATA",
        )
        .await?;

        // -- 3h: Recreate refresh policies
        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_hourly',
                start_offset => INTERVAL '3 hours',
                end_offset => INTERVAL '1 hour',
                schedule_interval => INTERVAL '1 hour')",
        )
        .await?;
        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_daily',
                start_offset => INTERVAL '3 days',
                end_offset => INTERVAL '1 day',
                schedule_interval => INTERVAL '1 day')",
        )
        .await?;
        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_weekly',
                start_offset => INTERVAL '3 weeks',
                end_offset => INTERVAL '1 week',
                schedule_interval => INTERVAL '1 week')",
        )
        .await?;
        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_monthly',
                start_offset => INTERVAL '3 months',
                end_offset => INTERVAL '1 month',
                schedule_interval => INTERVAL '1 month')",
        )
        .await?;

        // -- 3i: Recreate compression policy
        db.execute_unprepared(
            r"ALTER TABLE readings SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'site_id, parameter_id'
            )",
        )
        .await?;
        db.execute_unprepared("SELECT add_compression_policy('readings', INTERVAL '30 days')")
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // =================================================================
        // 1. Reverse readings changes
        // =================================================================

        // -- Remove compression policy + decompress
        db.execute_unprepared("SELECT remove_compression_policy('readings', if_exists => true)")
            .await?;
        db.execute_unprepared(
            r"DO $$
            DECLARE
                chunk REGCLASS;
            BEGIN
                FOR chunk IN
                    SELECT format('%I.%I', chunk_schema, chunk_name)::regclass
                    FROM timescaledb_information.chunks
                    WHERE hypertable_name = 'readings'
                      AND is_compressed = true
                LOOP
                    PERFORM decompress_chunk(chunk, if_compressed => true);
                END LOOP;
            END $$",
        )
        .await?;

        // -- Drop refresh policies
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_monthly', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_weekly', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_daily', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_hourly', if_exists => true)",
        )
        .await?;

        // -- Drop continuous aggregates
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_monthly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_weekly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_daily CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_hourly CASCADE")
            .await?;

        // -- Remove replicate_index: drop PK, drop column, recreate PK
        db.execute_unprepared(
            r"DO $$
            DECLARE
                pk_name TEXT;
            BEGIN
                SELECT conname INTO pk_name
                FROM pg_constraint
                WHERE conrelid = 'readings'::regclass
                  AND contype = 'p';
                IF pk_name IS NOT NULL THEN
                    EXECUTE format('ALTER TABLE readings DROP CONSTRAINT %I', pk_name);
                END IF;
            END $$",
        )
        .await?;
        db.execute_unprepared("ALTER TABLE readings DROP COLUMN IF EXISTS replicate_index")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE readings ADD PRIMARY KEY (site_id, parameter_id, time)",
        )
        .await?;

        // -- Recreate aggregates WITHOUT replicate_index filter
        db.execute_unprepared(
            r"CREATE MATERIALIZED VIEW IF NOT EXISTS readings_hourly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 hour', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE measurement_type = 'continuous' AND is_flagged IS NOT TRUE
            GROUP BY time_bucket('1 hour', time), site_id, parameter_id
            WITH NO DATA",
        )
        .await?;

        db.execute_unprepared(
            r"CREATE MATERIALIZED VIEW IF NOT EXISTS readings_daily
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 day', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE measurement_type = 'continuous' AND is_flagged IS NOT TRUE
            GROUP BY time_bucket('1 day', time), site_id, parameter_id
            WITH NO DATA",
        )
        .await?;

        db.execute_unprepared(
            r"CREATE MATERIALIZED VIEW IF NOT EXISTS readings_weekly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 week', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE measurement_type = 'continuous' AND is_flagged IS NOT TRUE
            GROUP BY time_bucket('1 week', time), site_id, parameter_id
            WITH NO DATA",
        )
        .await?;

        db.execute_unprepared(
            r"CREATE MATERIALIZED VIEW IF NOT EXISTS readings_monthly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 month', time) AS bucket,
                site_id,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            WHERE measurement_type = 'continuous' AND is_flagged IS NOT TRUE
            GROUP BY time_bucket('1 month', time), site_id, parameter_id
            WITH NO DATA",
        )
        .await?;

        // -- Recreate refresh policies
        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_hourly',
                start_offset => INTERVAL '3 hours',
                end_offset => INTERVAL '1 hour',
                schedule_interval => INTERVAL '1 hour')",
        )
        .await?;
        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_daily',
                start_offset => INTERVAL '3 days',
                end_offset => INTERVAL '1 day',
                schedule_interval => INTERVAL '1 day')",
        )
        .await?;
        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_weekly',
                start_offset => INTERVAL '3 weeks',
                end_offset => INTERVAL '1 week',
                schedule_interval => INTERVAL '1 week')",
        )
        .await?;
        db.execute_unprepared(
            r"SELECT add_continuous_aggregate_policy('readings_monthly',
                start_offset => INTERVAL '3 months',
                end_offset => INTERVAL '1 month',
                schedule_interval => INTERVAL '1 month')",
        )
        .await?;

        // -- Recreate compression policy
        db.execute_unprepared(
            r"ALTER TABLE readings SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'site_id, parameter_id'
            )",
        )
        .await?;
        db.execute_unprepared("SELECT add_compression_policy('readings', INTERVAL '30 days')")
            .await?;

        // =================================================================
        // 2. Reverse parameters changes
        // =================================================================

        db.execute_unprepared("ALTER TABLE parameters DROP COLUMN IF EXISTS default_warning_min")
            .await?;
        db.execute_unprepared("ALTER TABLE parameters DROP COLUMN IF EXISTS default_warning_max")
            .await?;
        db.execute_unprepared("ALTER TABLE parameters DROP COLUMN IF EXISTS default_alarm_min")
            .await?;
        db.execute_unprepared("ALTER TABLE parameters DROP COLUMN IF EXISTS default_alarm_max")
            .await?;

        // =================================================================
        // 3. Reverse alarm_thresholds changes
        // =================================================================

        // Drop new indexes
        db.execute_unprepared("DROP INDEX IF EXISTS uq_alarm_thresholds_param_site_type")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS uq_alarm_thresholds_param_null_site_type")
            .await?;

        // Drop alarm_type column
        db.execute_unprepared("ALTER TABLE alarm_thresholds DROP COLUMN IF EXISTS alarm_type")
            .await?;

        // Restore original indexes
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_alarm_thresholds_param_site ON alarm_thresholds (parameter_id, site_id) WHERE site_id IS NOT NULL",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_alarm_thresholds_param_global ON alarm_thresholds (parameter_id) WHERE site_id IS NULL",
        )
        .await?;

        Ok(())
    }
}
