use sea_orm_migration::prelude::*;

/// Consolidated clean-slate migration for the stream-based ingestion architecture.
///
/// Creates all 25 tables from scratch. This replaces the previous 6 migrations.
/// The user must drop the existing database before running this migration.
///
/// Key changes from previous schema:
/// - `source_mappings` and `sync_state` replaced by `data_streams`
/// - `readings` PK changed from (site_id, parameter_id, time, replicate_index) to (stream_id, time, replicate_index)
/// - `status_events` PK changed from (site_id, parameter_id, time) to (stream_id, time)
/// - `site_id` and `parameter_id` on readings/status_events are nullable (filled on pairing)
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // ====================================================================
        // Core hierarchy
        // ====================================================================

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS projects (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name VARCHAR(64) NOT NULL,
                description TEXT,
                data_source VARCHAR(64),
                is_public BOOLEAN DEFAULT FALSE,
                public_slug VARCHAR(64),
                public_api_title VARCHAR(128),
                public_api_description TEXT,
                public_api_version VARCHAR(32),
                public_contact_email VARCHAR(128),
                created_at TIMESTAMPTZ DEFAULT NOW(),
                discovered_at TIMESTAMPTZ
            );
            CREATE UNIQUE INDEX IF NOT EXISTS projects_name_lower_idx ON projects (LOWER(name));
            CREATE UNIQUE INDEX IF NOT EXISTS projects_public_slug_idx ON projects (public_slug) WHERE public_slug IS NOT NULL;
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS sites (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                project_id UUID REFERENCES projects(id),
                name VARCHAR(64) NOT NULL,
                latitude DOUBLE PRECISION,
                longitude DOUBLE PRECISION,
                altitude_m DOUBLE PRECISION,
                public_slug VARCHAR(64),
                created_at TIMESTAMPTZ DEFAULT NOW(),
                discovered_at TIMESTAMPTZ
            );
            CREATE UNIQUE INDEX IF NOT EXISTS sites_name_lower_idx ON sites (LOWER(name));
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS parameters (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name VARCHAR(64) NOT NULL,
                display_name VARCHAR(128) NOT NULL,
                default_units VARCHAR(32) NOT NULL DEFAULT '',
                category VARCHAR(32) NOT NULL DEFAULT 'measurement',
                data_type VARCHAR(32) NOT NULL DEFAULT 'numeric',
                description TEXT,
                default_warning_min DOUBLE PRECISION,
                default_warning_max DOUBLE PRECISION,
                default_alarm_min DOUBLE PRECISION,
                default_alarm_max DOUBLE PRECISION,
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            CREATE UNIQUE INDEX IF NOT EXISTS parameters_name_lower_idx ON parameters (LOWER(name));
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS derived_parameter_definitions (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name VARCHAR(128) NOT NULL UNIQUE,
                display_name VARCHAR(256),
                units VARCHAR(32),
                formula TEXT NOT NULL,
                description TEXT,
                required_parameter_types JSONB,
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS site_parameters (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id UUID NOT NULL REFERENCES sites(id),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                name VARCHAR(128) NOT NULL,
                sensor_type VARCHAR(64) NOT NULL DEFAULT '',
                display_units VARCHAR(32),
                units_name VARCHAR(64),
                units_min DOUBLE PRECISION,
                units_max DOUBLE PRECISION,
                decimal_places SMALLINT,
                channel_id INTEGER,
                sample_interval_sec INTEGER DEFAULT 600,
                is_active BOOLEAN DEFAULT TRUE,
                is_derived BOOLEAN DEFAULT FALSE,
                derived_definition_id UUID REFERENCES derived_parameter_definitions(id),
                variable_mappings JSONB,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                updated_at TIMESTAMPTZ DEFAULT NOW(),
                discovered_at TIMESTAMPTZ,
                CONSTRAINT uq_site_param UNIQUE (site_id, parameter_id),
                CONSTRAINT uq_site_param_name UNIQUE (site_id, name)
            );
            "#,
        )
        .await?;

        // ====================================================================
        // Sensors & calibrations
        // ====================================================================

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS sensors (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                serial_number VARCHAR(64),
                name VARCHAR(128),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                manufacturer VARCHAR(128),
                model VARCHAR(128),
                is_active BOOLEAN DEFAULT TRUE,
                is_lab_instrument BOOLEAN DEFAULT FALSE,
                notes TEXT,
                metadata JSONB,
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_sensors_serial_parameter
                ON sensors (serial_number, parameter_id) WHERE serial_number IS NOT NULL;
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS sensor_calibrations (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                sensor_id UUID NOT NULL REFERENCES sensors(id),
                slope DOUBLE PRECISION NOT NULL,
                intercept DOUBLE PRECISION NOT NULL,
                valid_from TIMESTAMPTZ NOT NULL,
                performed_by VARCHAR(128),
                notes TEXT,
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            CREATE INDEX IF NOT EXISTS idx_sensor_calibrations_sensor_valid
                ON sensor_calibrations (sensor_id, valid_from DESC);
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS sensor_deployments (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                sensor_id UUID NOT NULL REFERENCES sensors(id),
                site_id UUID NOT NULL REFERENCES sites(id),
                deployed_from TIMESTAMPTZ NOT NULL,
                deployed_until TIMESTAMPTZ,
                deployment_type VARCHAR(64) NOT NULL DEFAULT 'permanent',
                notes TEXT,
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS standard_curves (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                valid_from TIMESTAMPTZ NOT NULL,
                slope DOUBLE PRECISION NOT NULL,
                intercept DOUBLE PRECISION NOT NULL,
                r_squared DOUBLE PRECISION,
                notes TEXT,
                created_by VARCHAR(128),
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            "#,
        )
        .await?;

        // ====================================================================
        // Derived parameters
        // ====================================================================

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS derived_parameter_sources (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                derived_definition_id UUID NOT NULL REFERENCES derived_parameter_definitions(id),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                variable_name VARCHAR(64) NOT NULL,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                CONSTRAINT uq_derived_var UNIQUE (derived_definition_id, variable_name),
                CONSTRAINT uq_derived_param UNIQUE (derived_definition_id, parameter_id)
            );
            "#,
        )
        .await?;

        // ====================================================================
        // Pairing plans (first-class entity for auditable stream pairing)
        // ====================================================================

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS pairing_plans (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                source_system TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'draft',
                created_by TEXT,
                summary JSONB NOT NULL DEFAULT '{}',
                entries JSONB NOT NULL DEFAULT '[]',
                created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                applied_at TIMESTAMPTZ,
                apply_result JSONB
            );
            CREATE INDEX IF NOT EXISTS idx_pairing_plans_source_system ON pairing_plans (source_system);
            CREATE INDEX IF NOT EXISTS idx_pairing_plans_status ON pairing_plans (status);
            "#,
        )
        .await?;

        // ====================================================================
        // Data streams (replaces source_mappings + sync_state)
        // ====================================================================

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS data_streams (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                source_system TEXT NOT NULL,
                source_key TEXT NOT NULL,
                source_name TEXT,
                source_path TEXT,
                metadata JSONB NOT NULL DEFAULT '{}',
                site_parameter_id UUID REFERENCES site_parameters(id),
                sensor_id UUID REFERENCES sensors(id) ON DELETE SET NULL,
                pairing_plan_id UUID REFERENCES pairing_plans(id),
                is_active BOOLEAN NOT NULL DEFAULT TRUE,
                discovered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                paired_at TIMESTAMPTZ,
                last_data_time TIMESTAMPTZ,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                CONSTRAINT uq_stream_source UNIQUE (source_system, source_key)
            );
            CREATE INDEX IF NOT EXISTS idx_data_streams_source ON data_streams (source_system);
            CREATE INDEX IF NOT EXISTS idx_data_streams_site_param ON data_streams (site_parameter_id) WHERE site_parameter_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_data_streams_sensor_id ON data_streams (sensor_id) WHERE sensor_id IS NOT NULL;
            "#,
        )
        .await?;

        // ====================================================================
        // Field trips & annotations
        // ====================================================================

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS field_trips (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                date DATE NOT NULL,
                participants TEXT,
                notes TEXT,
                created_by VARCHAR(128),
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS annotations (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id UUID NOT NULL REFERENCES sites(id),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                start_time TIMESTAMPTZ NOT NULL,
                end_time TIMESTAMPTZ NOT NULL,
                text TEXT NOT NULL,
                category VARCHAR(64),
                created_by VARCHAR(128),
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            CREATE INDEX IF NOT EXISTS idx_annotations_site_param ON annotations (site_id, parameter_id);
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS notes (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id UUID NOT NULL REFERENCES sites(id),
                text TEXT NOT NULL,
                verified BOOLEAN NOT NULL DEFAULT FALSE,
                created_by VARCHAR(128),
                created_at TIMESTAMPTZ DEFAULT NOW(),
                updated_at TIMESTAMPTZ DEFAULT NOW()
            );
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS constants (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name VARCHAR(128) NOT NULL UNIQUE,
                value DOUBLE PRECISION NOT NULL,
                units VARCHAR(32),
                description TEXT,
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            "#,
        )
        .await?;

        // ====================================================================
        // Time-series hypertables
        // ====================================================================

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS readings (
                stream_id UUID NOT NULL REFERENCES data_streams(id),
                time TIMESTAMPTZ NOT NULL,
                replicate_index SMALLINT NOT NULL DEFAULT 0,
                site_id UUID REFERENCES sites(id),
                parameter_id UUID REFERENCES parameters(id),
                raw_value DOUBLE PRECISION NOT NULL,
                calibrated_value DOUBLE PRECISION,
                sensor_id UUID REFERENCES sensors(id),
                calibration_id UUID REFERENCES sensor_calibrations(id),
                deployment_id UUID REFERENCES sensor_deployments(id),
                logged BOOLEAN DEFAULT TRUE,
                measurement_type VARCHAR(32),
                is_flagged BOOLEAN DEFAULT FALSE,
                flag_reason TEXT,
                field_trip_id UUID REFERENCES field_trips(id),
                PRIMARY KEY (stream_id, time, replicate_index)
            );
            "#,
        )
        .await?;

        db.execute_unprepared(
            "SELECT create_hypertable('readings', 'time', chunk_time_interval => INTERVAL '7 days', if_not_exists => TRUE)",
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE INDEX IF NOT EXISTS idx_readings_site_param_time
                ON readings (site_id, parameter_id, time DESC)
                WHERE site_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_readings_stream_time
                ON readings (stream_id, time DESC);
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS status_events (
                stream_id UUID NOT NULL REFERENCES data_streams(id),
                time TIMESTAMPTZ NOT NULL,
                site_id UUID REFERENCES sites(id),
                parameter_id UUID REFERENCES parameters(id),
                value TEXT NOT NULL,
                sensor_id UUID REFERENCES sensors(id),
                PRIMARY KEY (stream_id, time)
            );
            "#,
        )
        .await?;

        db.execute_unprepared(
            "SELECT create_hypertable('status_events', 'time', chunk_time_interval => INTERVAL '30 days', if_not_exists => TRUE)",
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE INDEX IF NOT EXISTS idx_status_events_site_param_time
                ON status_events (site_id, parameter_id, time DESC)
                WHERE site_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_status_events_stream_time
                ON status_events (stream_id, time DESC);
            "#,
        )
        .await?;

        // ====================================================================
        // Alarms & visibility
        // ====================================================================

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS alarm_thresholds (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                site_id UUID REFERENCES sites(id),
                alarm_type VARCHAR(32) NOT NULL DEFAULT 'range',
                warning_min DOUBLE PRECISION,
                warning_max DOUBLE PRECISION,
                alarm_min DOUBLE PRECISION,
                alarm_max DOUBLE PRECISION,
                string_alarm_values JSONB,
                string_warning_values JSONB,
                description TEXT,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                updated_at TIMESTAMPTZ DEFAULT NOW()
            );
            CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_thresh_param_site_type
                ON alarm_thresholds (parameter_id, site_id, alarm_type) WHERE site_id IS NOT NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_thresh_param_type_global
                ON alarm_thresholds (parameter_id, alarm_type) WHERE site_id IS NULL;
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS api_tokens (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name VARCHAR(128) NOT NULL,
                token_hash VARCHAR(256) NOT NULL UNIQUE,
                project_scope UUID REFERENCES projects(id),
                permissions JSONB NOT NULL DEFAULT '{"read_metadata": true, "read_data": true, "write_metadata": false, "write_data": false}',
                is_active BOOLEAN NOT NULL DEFAULT TRUE,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                expires_at TIMESTAMPTZ,
                last_used_at TIMESTAMPTZ,
                created_by VARCHAR(128)
            );
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS public_exposed_parameters (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                project_id UUID NOT NULL REFERENCES projects(id),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                public_name VARCHAR(128),
                public_units VARCHAR(32),
                description TEXT,
                sort_order INTEGER DEFAULT 0,
                conversion_factor DOUBLE PRECISION DEFAULT 1.0,
                conversion_offset DOUBLE PRECISION DEFAULT 0.0,
                include_derived BOOLEAN DEFAULT FALSE,
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            "#,
        )
        .await?;

        // ====================================================================
        // Sync control plane
        // ====================================================================

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS sync_services (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_type VARCHAR(64) NOT NULL,
                instance_id VARCHAR(128) NOT NULL,
                status VARCHAR(32) NOT NULL DEFAULT 'registered',
                current_operation VARCHAR(128),
                last_heartbeat TIMESTAMPTZ,
                last_sync_completed_at TIMESTAMPTZ,
                last_error TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            );
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS sync_commands (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_id UUID NOT NULL REFERENCES sync_services(id),
                command VARCHAR(64) NOT NULL,
                payload JSONB,
                status VARCHAR(32) NOT NULL DEFAULT 'pending',
                result JSONB,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                expires_at TIMESTAMPTZ,
                acknowledged_at TIMESTAMPTZ,
                completed_at TIMESTAMPTZ
            );
            CREATE INDEX IF NOT EXISTS idx_sync_commands_pending
                ON sync_commands (service_id, status) WHERE status = 'pending';
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS sync_events (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_id UUID NOT NULL REFERENCES sync_services(id),
                command_id UUID REFERENCES sync_commands(id),
                event_type VARCHAR(64) NOT NULL,
                status VARCHAR(32) NOT NULL DEFAULT 'started',
                readings_synced BIGINT NOT NULL DEFAULT 0,
                status_events_synced BIGINT NOT NULL DEFAULT 0,
                errors JSONB,
                log JSONB,
                started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                completed_at TIMESTAMPTZ,
                duration_ms BIGINT DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_sync_events_service ON sync_events (service_id, started_at DESC);
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS sync_service_credentials (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                client_id VARCHAR(128) NOT NULL UNIQUE,
                client_secret_hash VARCHAR(256) NOT NULL,
                service_type VARCHAR(64) NOT NULL,
                service_id UUID REFERENCES sync_services(id),
                revoked BOOLEAN NOT NULL DEFAULT FALSE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            );
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS sync_service_tokens (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_id UUID NOT NULL REFERENCES sync_services(id),
                token_hash VARCHAR(256) NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            );
            CREATE INDEX IF NOT EXISTS idx_sync_tokens_hash ON sync_service_tokens (token_hash);
            "#,
        )
        .await?;

        // ====================================================================
        // Continuous aggregates (only include paired readings: site_id IS NOT NULL)
        // ====================================================================

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW IF NOT EXISTS readings_hourly
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
            WHERE site_id IS NOT NULL AND replicate_index = 0
            GROUP BY time_bucket('1 hour', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW IF NOT EXISTS readings_daily
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
            WHERE site_id IS NOT NULL AND replicate_index = 0
            GROUP BY time_bucket('1 day', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW IF NOT EXISTS readings_weekly
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
            WHERE site_id IS NOT NULL AND replicate_index = 0
            GROUP BY time_bucket('1 week', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE MATERIALIZED VIEW IF NOT EXISTS readings_monthly
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
            WHERE site_id IS NOT NULL AND replicate_index = 0
            GROUP BY time_bucket('1 month', time), site_id, parameter_id
            WITH NO DATA
            "#,
        )
        .await?;

        // Refresh policies
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

        // ====================================================================
        // Compression policies
        // ====================================================================

        db.execute_unprepared(
            r"ALTER TABLE readings SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'stream_id'
            )",
        )
        .await?;

        db.execute_unprepared("SELECT add_compression_policy('readings', INTERVAL '30 days')")
            .await?;

        db.execute_unprepared(
            r"ALTER TABLE status_events SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'stream_id'
            )",
        )
        .await?;

        db.execute_unprepared(
            "SELECT add_compression_policy('status_events', INTERVAL '90 days')",
        )
        .await?;

        // ====================================================================
        // Seed analytical parameters
        // ====================================================================

        db.execute_unprepared(
            r#"
            INSERT INTO parameters (id, name, display_name, default_units, category, data_type, description)
            VALUES
                (gen_random_uuid(), 'DOC_avg_ppb',           'DOC Average',               'ppb',     'DOC',        'numeric', 'Dissolved Organic Carbon average of replicates'),
                (gen_random_uuid(), 'DOC_sd_ppb',            'DOC Std Dev',               'ppb',     'DOC',        'numeric', 'Dissolved Organic Carbon standard deviation'),
                (gen_random_uuid(), 'TSS_dry_weight_mgL',    'TSS Dry Weight',            'mg/L',    'TSS',        'numeric', 'Total Suspended Solids from filter weights'),
                (gen_random_uuid(), 'AFDM_mgL',              'AFDM',                      'mg/L',    'TSS',        'numeric', 'Ash-Free Dry Mass'),
                (gen_random_uuid(), 'Chla_acid_ugL_avg',     'Chla Acid Average',         'ug/L',    'Chla',       'numeric', 'Chlorophyll-a (acid method) average'),
                (gen_random_uuid(), 'Chla_acid_ugL_sd',      'Chla Acid Std Dev',         'ug/L',    'Chla',       'numeric', 'Chlorophyll-a (acid method) standard deviation'),
                (gen_random_uuid(), 'Chla_noacid_ugL_avg',   'Chla Non-Acid Average',     'ug/L',    'Chla',       'numeric', 'Chlorophyll-a (non-acid method) average'),
                (gen_random_uuid(), 'Chla_noacid_ugL_sd',    'Chla Non-Acid Std Dev',     'ug/L',    'Chla',       'numeric', 'Chlorophyll-a (non-acid method) standard deviation'),
                (gen_random_uuid(), 'Chla_acid_ugm2_avg',    'Chla Acid per m2',          'ug/m2',   'Chla',       'numeric', 'Chlorophyll-a (acid) per square meter average'),
                (gen_random_uuid(), 'Chla_acid_ugm2_sd',     'Chla Acid per m2 SD',       'ug/m2',   'Chla',       'numeric', 'Chlorophyll-a (acid) per square meter std dev'),
                (gen_random_uuid(), 'Chla_noacid_ugm2_avg',  'Chla Non-Acid per m2',      'ug/m2',   'Chla',       'numeric', 'Chlorophyll-a (non-acid) per square meter average'),
                (gen_random_uuid(), 'Chla_noacid_ugm2_sd',   'Chla Non-Acid per m2 SD',   'ug/m2',   'Chla',       'numeric', 'Chlorophyll-a (non-acid) per square meter std dev'),
                (gen_random_uuid(), 'CO2_HS_Um_avg',         'CO2 Headspace Average',     'umol/L',  'pCO2',       'numeric', 'CO2 headspace concentration average'),
                (gen_random_uuid(), 'CO2_HS_Um_sd',          'CO2 Headspace SD',          'umol/L',  'pCO2',       'numeric', 'CO2 headspace concentration std dev'),
                (gen_random_uuid(), 'pCO2_HS_uatm_avg',      'pCO2 Average',              'uatm',    'pCO2',       'numeric', 'Partial pressure CO2 average'),
                (gen_random_uuid(), 'pCO2_HS_uatm_sd',       'pCO2 SD',                   'uatm',    'pCO2',       'numeric', 'Partial pressure CO2 std dev'),
                (gen_random_uuid(), 'pCO2_HS_P1_uatm_avg',   'pCO2 P1 Average',           'uatm',    'pCO2',       'numeric', 'pCO2 method P1 average'),
                (gen_random_uuid(), 'pCO2_HS_P1_uatm_sd',    'pCO2 P1 SD',                'uatm',    'pCO2',       'numeric', 'pCO2 method P1 std dev'),
                (gen_random_uuid(), 'pCO2_HS_P2_uatm_avg',   'pCO2 P2 Average',           'uatm',    'pCO2',       'numeric', 'pCO2 method P2 average'),
                (gen_random_uuid(), 'pCO2_HS_P2_uatm_sd',    'pCO2 P2 SD',                'uatm',    'pCO2',       'numeric', 'pCO2 method P2 std dev'),
                (gen_random_uuid(), 'd13C_CO2_avg',          'd13C-CO2 Average',          'permil',  'pCO2',       'numeric', 'd13C of CO2 average'),
                (gen_random_uuid(), 'd13C_CO2_sd',           'd13C-CO2 SD',               'permil',  'pCO2',       'numeric', 'd13C of CO2 std dev'),
                (gen_random_uuid(), 'CH4_umol_L_avg',        'CH4 Dissolved Average',     'umol/L',  'pCO2',       'numeric', 'Dissolved CH4 average'),
                (gen_random_uuid(), 'CH4_umol_L_sd',         'CH4 Dissolved SD',          'umol/L',  'pCO2',       'numeric', 'Dissolved CH4 std dev'),
                (gen_random_uuid(), 'DIC_avg',               'DIC Average',               'umol/L',  'DIC',        'numeric', 'Dissolved Inorganic Carbon average'),
                (gen_random_uuid(), 'DIC_std',               'DIC Std Dev',               'umol/L',  'DIC',        'numeric', 'Dissolved Inorganic Carbon std dev'),
                (gen_random_uuid(), 'd13C_DIC_avg',          'd13C-DIC Average',          'permil',  'DIC',        'numeric', 'd13C of DIC average'),
                (gen_random_uuid(), 'd13C_DIC_std',          'd13C-DIC Std Dev',          'permil',  'DIC',        'numeric', 'd13C of DIC std dev'),
                (gen_random_uuid(), 'SUVA',                  'SUVA',                      'L/mg*m',  'DOM',        'numeric', 'Specific UV Absorbance at 254nm'),
                (gen_random_uuid(), 'A_T',                   'A/T Ratio',                 'ratio',   'DOM',        'numeric', 'DOM fluorescence peak ratio A/T'),
                (gen_random_uuid(), 'C_A',                   'C/A Ratio',                 'ratio',   'DOM',        'numeric', 'DOM fluorescence peak ratio C/A'),
                (gen_random_uuid(), 'C_M',                   'C/M Ratio',                 'ratio',   'DOM',        'numeric', 'DOM fluorescence peak ratio C/M'),
                (gen_random_uuid(), 'C_T',                   'C/T Ratio',                 'ratio',   'DOM',        'numeric', 'DOM fluorescence peak ratio C/T'),
                (gen_random_uuid(), 'NUT_P_avg',             'PO4 Average',               'ug/L',    'Nutrients',  'numeric', 'Phosphate average of replicates'),
                (gen_random_uuid(), 'NUT_P_sd',              'PO4 Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Phosphate std dev'),
                (gen_random_uuid(), 'NUT_NH4_avg',           'NH4 Average',               'ug/L',    'Nutrients',  'numeric', 'Ammonium average of replicates'),
                (gen_random_uuid(), 'NUT_NH4_sd',            'NH4 Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Ammonium std dev'),
                (gen_random_uuid(), 'NUT_NOx_avg',           'NOx Average',               'ug/L',    'Nutrients',  'numeric', 'Nitrate+Nitrite average of replicates'),
                (gen_random_uuid(), 'NUT_NOx_sd',            'NOx Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Nitrate+Nitrite std dev'),
                (gen_random_uuid(), 'NUT_NO2_avg',           'NO2 Average',               'ug/L',    'Nutrients',  'numeric', 'Nitrite average of replicates'),
                (gen_random_uuid(), 'NUT_NO2_sd',            'NO2 Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Nitrite std dev'),
                (gen_random_uuid(), 'NUT_NO3_avg',           'NO3 Average',               'ug/L',    'Nutrients',  'numeric', 'Nitrate average (NOx - NO2)'),
                (gen_random_uuid(), 'NUT_NO3_sd',            'NO3 Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Nitrate std dev'),
                (gen_random_uuid(), 'NUT_TDP_avg',           'TDP Average',               'ug/L',    'Nutrients',  'numeric', 'Total Dissolved Phosphorus average'),
                (gen_random_uuid(), 'NUT_TDP_sd',            'TDP Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Total Dissolved Phosphorus std dev'),
                (gen_random_uuid(), 'NUT_TDN_avg',           'TDN Average',               'ug/L',    'Nutrients',  'numeric', 'Total Dissolved Nitrogen average'),
                (gen_random_uuid(), 'NUT_TDN_sd',            'TDN Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Total Dissolved Nitrogen std dev'),
                (gen_random_uuid(), 'Field_BP_altitude',     'Barometric Pressure (alt)', 'hPa',     'Field data', 'numeric', 'Barometric pressure calculated from altitude'),
                (gen_random_uuid(), 'Vaisala_CO2_min_corr',  'Vaisala CO2 Min Corrected', 'ppm',     'Field data', 'numeric', 'Vaisala CO2 minimum corrected for T/P'),
                (gen_random_uuid(), 'Vaisala_CO2_avg_corr',  'Vaisala CO2 Avg Corrected', 'ppm',     'Field data', 'numeric', 'Vaisala CO2 average corrected for T/P'),
                (gen_random_uuid(), 'Vaisala_CO2_max_corr',  'Vaisala CO2 Max Corrected', 'ppm',     'Field data', 'numeric', 'Vaisala CO2 maximum corrected for T/P'),
                (gen_random_uuid(), 'Reach_depth_avg_cm',    'Reach Depth Average',       'cm',      'Field data', 'numeric', 'Average of reach depth replicates'),
                (gen_random_uuid(), 'Reach_depth_sd_cm',     'Reach Depth Std Dev',       'cm',      'Field data', 'numeric', 'Std dev of reach depth replicates'),
                (gen_random_uuid(), 'lab_co2air_ch4_dry',    'CH4 Dry (Air)',             'ppm',     'CO2_air',    'numeric', 'CH4 dry concentration from wet measurement'),
                (gen_random_uuid(), 'lab_co2air_co2_dry',    'CO2 Dry (Air)',             'ppm',     'CO2_air',    'numeric', 'CO2 dry concentration from wet measurement'),
                (gen_random_uuid(), 'benthic_AFDM_avg_gm2',  'Benthic AFDM Average',     'g/m2',    'Benthic',    'numeric', 'Benthic AFDM per square meter average'),
                (gen_random_uuid(), 'benthic_AFDM_sd_gm2',   'Benthic AFDM Std Dev',     'g/m2',    'Benthic',    'numeric', 'Benthic AFDM per square meter std dev'),
                (gen_random_uuid(), 'd_excess',              'Deuterium Excess',          'permil',  'Isotopes',   'numeric', 'Deuterium excess (dD - 8*d18O)'),
                (gen_random_uuid(), 'o17_excess_permeg',     '17O Excess',                'per_meg', 'Isotopes',   'numeric', '17-oxygen excess in per meg'),
                (gen_random_uuid(), 'alkalinity_meq_l',      'Alkalinity (meq/L)',        'meq/L',   'Alkalinity', 'numeric', 'Gran titration alkalinity'),
                (gen_random_uuid(), 'alkalinity_mg_l_caco3', 'Alkalinity (CaCO3)',        'mg/L',    'Alkalinity', 'numeric', 'Alkalinity as mg/L CaCO3'),
                (gen_random_uuid(), 'sum_cations_meq',       'Cations Sum',               'meq/L',   'Ions',       'numeric', 'Sum of cation charge equivalents'),
                (gen_random_uuid(), 'sum_anions_meq',        'Anions Sum',                'meq/L',   'Ions',       'numeric', 'Sum of anion charge equivalents'),
                (gen_random_uuid(), 'balance_percent',       'Ion Balance',               '%',       'Ions',       'numeric', 'Ion charge balance percentage')
            ON CONFLICT (LOWER(name)) DO NOTHING;
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Remove compression policies
        db.execute_unprepared("SELECT remove_compression_policy('readings', if_exists => true)")
            .await
            .ok();
        db.execute_unprepared(
            "SELECT remove_compression_policy('status_events', if_exists => true)",
        )
        .await
        .ok();

        // Remove continuous aggregate policies and views
        for view in &[
            "readings_monthly",
            "readings_weekly",
            "readings_daily",
            "readings_hourly",
        ] {
            db.execute_unprepared(&format!(
                "SELECT remove_continuous_aggregate_policy('{view}', if_exists => true)"
            ))
            .await
            .ok();
            db.execute_unprepared(&format!("DROP MATERIALIZED VIEW IF EXISTS {view} CASCADE"))
                .await?;
        }

        // Drop tables in reverse dependency order
        for table in &[
            "sync_service_tokens",
            "sync_service_credentials",
            "sync_events",
            "sync_commands",
            "sync_services",
            "public_exposed_parameters",
            "api_tokens",
            "alarm_thresholds",
            "status_events",
            "readings",
            "constants",
            "notes",
            "annotations",
            "field_trips",
            "data_streams",
            "pairing_plans",
            "derived_parameter_sources",
            "standard_curves",
            "sensor_deployments",
            "sensor_calibrations",
            "sensors",
            "site_parameters",
            "derived_parameter_definitions",
            "parameters",
            "sites",
            "projects",
        ] {
            db.execute_unprepared(&format!("DROP TABLE IF EXISTS {table} CASCADE"))
                .await?;
        }

        Ok(())
    }
}
