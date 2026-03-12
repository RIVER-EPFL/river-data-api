use sea_orm_migration::prelude::*;

/// Comprehensive upgrade migration: v0.2.0 → HEAD.
///
/// Transforms the v0.2.0 schema (site-specific parameters, device_status table,
/// readings keyed by parameter_id) into the HEAD schema (global parameter catalog,
/// site_parameters, sensors, calibrations, readings keyed by site_id+parameter_id,
/// status_events, sync control plane, annotations, notes, standard curves,
/// constants, field trips, reading flags, and grab sample support).
///
/// Uses raw SQL throughout because TimescaleDB DDL (hypertable recreation,
/// continuous aggregates) cannot be expressed in the SeaORM table builder API.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // =====================================================================
        // Phase A: Add columns to existing tables
        // =====================================================================

        // -- projects: +7 public API columns
        db.execute_unprepared(
            "ALTER TABLE projects ADD COLUMN IF NOT EXISTS data_source VARCHAR(64) NOT NULL DEFAULT 'vaisala'",
        ).await?;
        db.execute_unprepared(
            "ALTER TABLE projects ADD COLUMN IF NOT EXISTS is_public BOOLEAN NOT NULL DEFAULT false",
        ).await?;
        db.execute_unprepared(
            "ALTER TABLE projects ADD COLUMN IF NOT EXISTS public_slug VARCHAR(64) UNIQUE",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE projects ADD COLUMN IF NOT EXISTS public_api_title VARCHAR(128)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE projects ADD COLUMN IF NOT EXISTS public_api_description TEXT",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE projects ADD COLUMN IF NOT EXISTS public_api_version VARCHAR(16) DEFAULT '1.0.0'",
        ).await?;
        db.execute_unprepared(
            "ALTER TABLE projects ADD COLUMN IF NOT EXISTS public_contact_email VARCHAR(128)",
        )
        .await?;

        // -- sites: +public_slug
        db.execute_unprepared("ALTER TABLE sites ADD COLUMN IF NOT EXISTS public_slug VARCHAR(64)")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS sites_project_public_slug_idx ON sites (project_id, public_slug) WHERE public_slug IS NOT NULL",
        ).await?;

        // -- source_mappings: +source_system, +entity_id index
        db.execute_unprepared(
            "ALTER TABLE source_mappings ADD COLUMN IF NOT EXISTS source_system VARCHAR(64) DEFAULT 'vaisala'",
        ).await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_source_mappings_entity_id ON source_mappings (entity_id)",
        ).await?;

        // =====================================================================
        // Phase B: Rename old parameters → old_parameters
        // =====================================================================

        db.execute_unprepared("ALTER TABLE parameters RENAME TO old_parameters")
            .await?;
        db.execute_unprepared(
            "ALTER INDEX IF EXISTS idx_parameters_site_name RENAME TO idx_old_parameters_site_name",
        )
        .await?;

        // =====================================================================
        // Phase C: Create new tables
        // =====================================================================

        // -- C1: parameters (global catalog)
        db.execute_unprepared(
            r"CREATE TABLE IF NOT EXISTS parameters (
                id UUID NOT NULL PRIMARY KEY DEFAULT gen_random_uuid(),
                name VARCHAR(64) NOT NULL UNIQUE,
                display_name VARCHAR(128) NOT NULL,
                default_units VARCHAR(32) NOT NULL,
                category VARCHAR(32) NOT NULL DEFAULT 'measurement',
                data_type VARCHAR(16) NOT NULL DEFAULT 'numeric',
                description TEXT,
                created_at TIMESTAMPTZ DEFAULT NOW()
            )",
        )
        .await?;

        // -- C2: sensors
        db.execute_unprepared(
            r"CREATE TABLE IF NOT EXISTS sensors (
                id UUID NOT NULL PRIMARY KEY DEFAULT gen_random_uuid(),
                serial_number VARCHAR(64),
                name VARCHAR(128),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                manufacturer VARCHAR(128),
                model VARCHAR(128),
                is_active BOOLEAN DEFAULT true,
                is_lab_instrument BOOLEAN DEFAULT false,
                notes TEXT,
                created_at TIMESTAMPTZ DEFAULT NOW()
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS sensors_serial_param_idx ON sensors (serial_number, parameter_id) WHERE serial_number IS NOT NULL",
        ).await?;

        // -- C3: sensor_calibrations
        db.execute_unprepared(
            r"CREATE TABLE IF NOT EXISTS sensor_calibrations (
                id UUID NOT NULL PRIMARY KEY DEFAULT gen_random_uuid(),
                sensor_id UUID NOT NULL REFERENCES sensors(id),
                slope DOUBLE PRECISION NOT NULL,
                intercept DOUBLE PRECISION NOT NULL,
                valid_from TIMESTAMPTZ NOT NULL,
                performed_by VARCHAR(128),
                notes TEXT,
                created_at TIMESTAMPTZ DEFAULT NOW()
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sensor_calibrations_sensor_valid_from ON sensor_calibrations (sensor_id, valid_from DESC)",
        ).await?;

        // -- C4: derived_parameter_definitions
        db.execute_unprepared(
            r"CREATE TABLE IF NOT EXISTS derived_parameter_definitions (
                id UUID NOT NULL PRIMARY KEY DEFAULT gen_random_uuid(),
                name VARCHAR(128) NOT NULL UNIQUE,
                display_name VARCHAR(256) NOT NULL,
                units VARCHAR(32) NOT NULL,
                formula TEXT NOT NULL,
                description TEXT,
                required_parameter_types JSONB NOT NULL DEFAULT '[]',
                created_at TIMESTAMPTZ DEFAULT NOW()
            )",
        )
        .await?;

        // -- C5: site_parameters
        db.execute_unprepared(
            r"CREATE TABLE IF NOT EXISTS site_parameters (
                id UUID NOT NULL PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id UUID NOT NULL REFERENCES sites(id),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                name VARCHAR(64) NOT NULL,
                sensor_type VARCHAR(64) NOT NULL,
                display_units VARCHAR(32),
                units_name VARCHAR(64),
                units_min DOUBLE PRECISION,
                units_max DOUBLE PRECISION,
                decimal_places SMALLINT,
                channel_id INTEGER,
                sample_interval_sec INTEGER DEFAULT 600,
                is_active BOOLEAN DEFAULT true,
                is_derived BOOLEAN DEFAULT false,
                derived_definition_id UUID REFERENCES derived_parameter_definitions(id),
                variable_mappings JSONB,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                updated_at TIMESTAMPTZ DEFAULT NOW(),
                discovered_at TIMESTAMPTZ
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_site_parameters_site_param ON site_parameters (site_id, parameter_id)",
        ).await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_site_parameters_site_name ON site_parameters (site_id, name)",
        ).await?;

        // -- C6: sensor_deployments
        db.execute_unprepared(
            r"CREATE TABLE IF NOT EXISTS sensor_deployments (
                id UUID NOT NULL PRIMARY KEY DEFAULT gen_random_uuid(),
                sensor_id UUID NOT NULL REFERENCES sensors(id),
                site_id UUID NOT NULL REFERENCES sites(id),
                deployed_from TIMESTAMPTZ NOT NULL,
                deployed_until TIMESTAMPTZ,
                deployment_type VARCHAR(32) NOT NULL DEFAULT 'permanent',
                notes TEXT,
                created_at TIMESTAMPTZ DEFAULT NOW()
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sensor_deployments_sensor_from ON sensor_deployments (sensor_id, deployed_from DESC)",
        ).await?;

        // -- C7: api_tokens
        db.execute_unprepared(
            r"CREATE TABLE IF NOT EXISTS api_tokens (
                id UUID NOT NULL PRIMARY KEY DEFAULT gen_random_uuid(),
                name VARCHAR(128) NOT NULL,
                token_hash VARCHAR(64) NOT NULL UNIQUE,
                project_scope UUID REFERENCES projects(id),
                permissions JSONB NOT NULL DEFAULT '{}',
                is_active BOOLEAN NOT NULL DEFAULT true,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                expires_at TIMESTAMPTZ,
                last_used_at TIMESTAMPTZ,
                created_by VARCHAR(128)
            )",
        )
        .await?;

        // -- C8: public_exposed_parameters
        db.execute_unprepared(
            r"CREATE TABLE IF NOT EXISTS public_exposed_parameters (
                id UUID NOT NULL PRIMARY KEY DEFAULT gen_random_uuid(),
                project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                public_name VARCHAR(64) NOT NULL,
                public_units VARCHAR(32) NOT NULL,
                description TEXT,
                sort_order INTEGER NOT NULL DEFAULT 0,
                conversion_factor DOUBLE PRECISION DEFAULT 1.0,
                conversion_offset DOUBLE PRECISION DEFAULT 0.0,
                include_derived BOOLEAN NOT NULL DEFAULT false,
                created_at TIMESTAMPTZ DEFAULT NOW()
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_public_exposed_params_project_name ON public_exposed_parameters (project_id, public_name)",
        ).await?;

        // -- C9: status_events (hypertable)
        db.execute_unprepared(
            r"CREATE TABLE IF NOT EXISTS status_events (
                site_id UUID NOT NULL,
                parameter_id UUID NOT NULL,
                time TIMESTAMPTZ NOT NULL,
                value TEXT NOT NULL,
                sensor_id UUID,
                PRIMARY KEY (site_id, parameter_id, time)
            )",
        )
        .await?;
        db.execute_unprepared(
            "SELECT create_hypertable('status_events', 'time', chunk_time_interval => INTERVAL '30 days', if_not_exists => TRUE)",
        ).await?;

        // -- C10: sync_services (service registry)
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS sync_services (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_type TEXT NOT NULL,
                instance_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'starting',
                current_operation TEXT,
                last_heartbeat TIMESTAMPTZ,
                last_sync_completed_at TIMESTAMPTZ,
                last_error TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                UNIQUE(service_type, instance_id)
            )",
        )
        .await?;

        // -- C11: sync_commands (command queue)
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS sync_commands (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_id UUID NOT NULL REFERENCES sync_services(id) ON DELETE CASCADE,
                command TEXT NOT NULL,
                payload JSONB,
                status TEXT NOT NULL DEFAULT 'pending',
                result JSONB,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                expires_at TIMESTAMPTZ NOT NULL DEFAULT NOW() + INTERVAL '5 minutes',
                acknowledged_at TIMESTAMPTZ,
                completed_at TIMESTAMPTZ
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sync_commands_pending
             ON sync_commands(service_id, status) WHERE status = 'pending'",
        )
        .await?;

        // -- C12: sync_service_credentials
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS sync_service_credentials (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                client_id TEXT NOT NULL UNIQUE,
                client_secret_hash TEXT NOT NULL,
                service_type TEXT NOT NULL,
                service_id UUID REFERENCES sync_services(id),
                revoked BOOLEAN NOT NULL DEFAULT FALSE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
        )
        .await?;

        // -- C13: sync_service_tokens
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS sync_service_tokens (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_id UUID NOT NULL REFERENCES sync_services(id) ON DELETE CASCADE,
                token_hash TEXT NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sync_service_tokens_lookup
             ON sync_service_tokens(token_hash)",
        )
        .await?;

        // -- C14: annotations
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS annotations (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id UUID NOT NULL REFERENCES sites(id),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                start_time TIMESTAMPTZ NOT NULL,
                end_time TIMESTAMPTZ NOT NULL,
                text TEXT NOT NULL,
                category VARCHAR(50) NOT NULL DEFAULT 'other',
                created_by VARCHAR(255),
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"#,
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_annotations_site_param ON annotations(site_id, parameter_id)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_annotations_time ON annotations(start_time, end_time)",
        )
        .await?;

        // -- C15: notes
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS notes (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id UUID NOT NULL REFERENCES sites(id),
                text TEXT NOT NULL,
                verified BOOLEAN NOT NULL DEFAULT FALSE,
                created_by VARCHAR(255),
                created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_notes_site ON notes(site_id)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_notes_created ON notes(created_at DESC)",
        )
        .await?;

        // -- C16: standard_curves
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS standard_curves (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                valid_from TIMESTAMPTZ NOT NULL,
                slope DOUBLE PRECISION NOT NULL,
                intercept DOUBLE PRECISION NOT NULL,
                r_squared DOUBLE PRECISION,
                notes TEXT,
                created_by VARCHAR(255),
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_standard_curves_param ON standard_curves(parameter_id)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_standard_curves_valid ON standard_curves(valid_from DESC)",
        )
        .await?;

        // -- C17: constants (with seed data)
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS constants (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name VARCHAR(255) NOT NULL UNIQUE,
                value DOUBLE PRECISION NOT NULL,
                units VARCHAR(100),
                description TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO constants (name, value, units, description) VALUES
                ('gas_constant', 8.314, 'J/(mol\u{00b7}K)', 'Universal gas constant R'),
                ('barometric_coefficient_a', 101325, 'Pa', 'Standard atmospheric pressure at sea level'),
                ('barometric_exponent', 5.25588, NULL, 'Exponent in barometric formula'),
                ('barometric_altitude_coeff', 2.25577e-5, '1/m', 'Altitude coefficient in barometric formula'),
                ('water_density_4c', 999.97, 'kg/m\u{00b3}', 'Density of water at 4\u{00b0}C')
            ON CONFLICT (name) DO NOTHING",
        )
        .await?;

        // -- C18: field_trips
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS field_trips (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                date DATE NOT NULL,
                participants TEXT,
                notes TEXT,
                created_by VARCHAR(255),
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_field_trips_date ON field_trips(date DESC)",
        )
        .await?;

        // =====================================================================
        // Phase D: Populate new tables from old data
        // =====================================================================

        // -- D1: Global parameters from distinct sensor_types (measurement params)
        db.execute_unprepared(
            r"INSERT INTO parameters (name, display_name, default_units, category, data_type)
            SELECT DISTINCT sensor_type, sensor_type, COALESCE(display_units, 'unknown'), 'measurement', 'numeric'
            FROM old_parameters
            ON CONFLICT (name) DO NOTHING",
        ).await?;

        // -- D2: Device health global parameters
        db.execute_unprepared(
            r"INSERT INTO parameters (name, display_name, default_units, category, data_type) VALUES
            ('Battery_Level', 'Battery Level', '%', 'device_health', 'integer'),
            ('Battery_State', 'Battery State', '-', 'device_health', 'integer'),
            ('Signal_Quality', 'Signal Quality', '-', 'device_health', 'integer'),
            ('Unreachable', 'Unreachable', '-', 'device_health', 'boolean'),
            ('Device_Status', 'Device Status', '-', 'device_health', 'string')
            ON CONFLICT (name) DO NOTHING",
        )
        .await?;

        // -- D3: site_parameters, PRESERVING old parameter UUIDs as site_parameter IDs
        db.execute_unprepared(
            r"INSERT INTO site_parameters (
                id, site_id, parameter_id, name, sensor_type,
                display_units, units_name, units_min, units_max,
                decimal_places, channel_id, sample_interval_sec,
                is_active, created_at, updated_at, discovered_at
            )
            SELECT op.id, op.site_id, p.id, op.name, op.sensor_type,
                op.display_units, op.units_name, op.units_min, op.units_max,
                op.decimal_places, op.channel_id, op.sample_interval_sec,
                op.is_active, op.created_at, op.updated_at, op.discovered_at
            FROM old_parameters op
            JOIN parameters p ON p.name = op.sensor_type",
        )
        .await?;

        // -- D4: Device health site_parameters (one per device health metric per site)
        db.execute_unprepared(
            r"INSERT INTO site_parameters (site_id, parameter_id, name, sensor_type, is_active)
            SELECT DISTINCT s.id, p.id,
                CONCAT(LEFT(s.name, 1), '_', p.name),
                p.name, true
            FROM sites s
            CROSS JOIN parameters p
            WHERE p.category = 'device_health'
            ON CONFLICT DO NOTHING",
        )
        .await?;

        // -- D5: Sensors from old device_serial_number
        db.execute_unprepared(
            r"INSERT INTO sensors (serial_number, parameter_id, is_active)
            SELECT DISTINCT ON (op.device_serial_number, p.id)
                op.device_serial_number, p.id, true
            FROM old_parameters op
            JOIN parameters p ON p.name = op.sensor_type
            WHERE op.device_serial_number IS NOT NULL",
        )
        .await?;

        // -- D6: Initial sensor deployments
        db.execute_unprepared(
            r"INSERT INTO sensor_deployments (sensor_id, site_id, deployed_from, deployment_type)
            SELECT s.id, sp.site_id, COALESCE(sp.discovered_at, sp.created_at, NOW()), 'permanent'
            FROM site_parameters sp
            JOIN old_parameters op ON op.id = sp.id
            JOIN sensors s ON s.serial_number = op.device_serial_number AND s.parameter_id = sp.parameter_id
            WHERE op.device_serial_number IS NOT NULL",
        ).await?;

        // =====================================================================
        // Phase E: Migrate readings hypertable
        // =====================================================================

        // -- E1: Remove compression + decompress all chunks
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

        // -- E2: Drop continuous aggregates
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
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_monthly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_weekly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_daily CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_hourly CASCADE")
            .await?;

        // -- E3: Rename old readings
        db.execute_unprepared("ALTER TABLE readings RENAME TO old_readings")
            .await?;

        // -- E4: Create new readings table with all columns + hypertable
        db.execute_unprepared(
            r"CREATE TABLE readings (
                site_id UUID NOT NULL,
                parameter_id UUID NOT NULL,
                time TIMESTAMPTZ NOT NULL,
                raw_value DOUBLE PRECISION NOT NULL,
                calibrated_value DOUBLE PRECISION,
                sensor_id UUID,
                calibration_id UUID,
                deployment_id UUID,
                logged BOOLEAN DEFAULT true,
                measurement_type VARCHAR(20) NOT NULL DEFAULT 'continuous',
                is_flagged BOOLEAN DEFAULT FALSE,
                flag_reason TEXT,
                field_trip_id UUID,
                PRIMARY KEY (site_id, parameter_id, time),
                CONSTRAINT fk_readings_site FOREIGN KEY (site_id) REFERENCES sites(id),
                CONSTRAINT fk_readings_parameter FOREIGN KEY (parameter_id) REFERENCES parameters(id),
                CONSTRAINT fk_readings_sensor FOREIGN KEY (sensor_id) REFERENCES sensors(id),
                CONSTRAINT fk_readings_calibration FOREIGN KEY (calibration_id) REFERENCES sensor_calibrations(id),
                CONSTRAINT fk_readings_deployment FOREIGN KEY (deployment_id) REFERENCES sensor_deployments(id),
                CONSTRAINT readings_measurement_type_check CHECK (measurement_type IN ('continuous', 'spot', 'derived'))
            )",
        ).await?;
        db.execute_unprepared(
            "SELECT create_hypertable('readings', 'time', chunk_time_interval => INTERVAL '7 days', if_not_exists => TRUE)",
        ).await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_readings_site_param_time ON readings (site_id, parameter_id, time DESC)",
        ).await?;

        // -- E5: Migrate measurement readings
        db.execute_unprepared(
            r"INSERT INTO readings (site_id, parameter_id, time, raw_value, logged)
            SELECT op.site_id, p.id, r.time, r.value, r.logged
            FROM old_readings r
            JOIN old_parameters op ON op.id = r.parameter_id
            JOIN parameters p ON p.name = op.sensor_type",
        )
        .await?;

        // -- E6: Migrate device_status numeric data → readings
        // Battery_Level
        db.execute_unprepared(
            r"INSERT INTO readings (site_id, parameter_id, time, raw_value)
            SELECT op.site_id, bp.id, ds.time, ds.battery_level::double precision
            FROM device_status ds
            JOIN old_parameters op ON op.id = ds.parameter_id
            JOIN parameters bp ON bp.name = 'Battery_Level'
            WHERE ds.battery_level IS NOT NULL
            ON CONFLICT DO NOTHING",
        )
        .await?;

        // Battery_State
        db.execute_unprepared(
            r"INSERT INTO readings (site_id, parameter_id, time, raw_value)
            SELECT op.site_id, bp.id, ds.time, ds.battery_state::double precision
            FROM device_status ds
            JOIN old_parameters op ON op.id = ds.parameter_id
            JOIN parameters bp ON bp.name = 'Battery_State'
            WHERE ds.battery_state IS NOT NULL
            ON CONFLICT DO NOTHING",
        )
        .await?;

        // Signal_Quality
        db.execute_unprepared(
            r"INSERT INTO readings (site_id, parameter_id, time, raw_value)
            SELECT op.site_id, bp.id, ds.time, ds.signal_quality::double precision
            FROM device_status ds
            JOIN old_parameters op ON op.id = ds.parameter_id
            JOIN parameters bp ON bp.name = 'Signal_Quality'
            WHERE ds.signal_quality IS NOT NULL
            ON CONFLICT DO NOTHING",
        )
        .await?;

        // Unreachable (bool → 0.0/1.0)
        db.execute_unprepared(
            r"INSERT INTO readings (site_id, parameter_id, time, raw_value)
            SELECT op.site_id, bp.id, ds.time, CASE WHEN ds.unreachable THEN 1.0 ELSE 0.0 END
            FROM device_status ds
            JOIN old_parameters op ON op.id = ds.parameter_id
            JOIN parameters bp ON bp.name = 'Unreachable'
            WHERE ds.unreachable IS NOT NULL
            ON CONFLICT DO NOTHING",
        )
        .await?;

        // -- E7: Migrate device_status string data → status_events
        db.execute_unprepared(
            r"INSERT INTO status_events (site_id, parameter_id, time, value)
            SELECT op.site_id, dp.id, ds.time, ds.device_status
            FROM device_status ds
            JOIN old_parameters op ON op.id = ds.parameter_id
            JOIN parameters dp ON dp.name = 'Device_Status'
            WHERE ds.device_status IS NOT NULL",
        )
        .await?;

        // -- E8: Drop old readings
        db.execute_unprepared("DROP TABLE old_readings").await?;

        // -- E9: Create continuous aggregates (with measurement_type + flag filters)
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

        // Compression policy
        db.execute_unprepared(
            r"ALTER TABLE readings SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'site_id, parameter_id'
            )",
        )
        .await?;
        db.execute_unprepared("SELECT add_compression_policy('readings', INTERVAL '30 days')")
            .await?;

        // =====================================================================
        // Phase F: Migrate sync_state
        // =====================================================================

        db.execute_unprepared("ALTER TABLE sync_state RENAME TO old_sync_state")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE old_sync_state DROP CONSTRAINT IF EXISTS fk_sync_state_parameter",
        )
        .await?;

        db.execute_unprepared(
            r"CREATE TABLE sync_state (
                site_parameter_id UUID NOT NULL PRIMARY KEY,
                last_data_time TIMESTAMPTZ,
                last_sync_attempt TIMESTAMPTZ,
                sync_status VARCHAR(32) DEFAULT 'pending',
                error_message TEXT,
                retry_count INTEGER DEFAULT 0,
                last_full_sync TIMESTAMPTZ,
                CONSTRAINT fk_sync_state_site_parameter FOREIGN KEY (site_parameter_id) REFERENCES site_parameters(id)
            )",
        ).await?;

        // Old parameter_id = new site_parameter_id (UUIDs preserved!)
        db.execute_unprepared(
            r"INSERT INTO sync_state (site_parameter_id, last_data_time, last_sync_attempt, sync_status, error_message, retry_count, last_full_sync)
            SELECT parameter_id, last_data_time, last_sync_attempt, sync_status, error_message, retry_count, last_full_sync
            FROM old_sync_state",
        ).await?;

        db.execute_unprepared("DROP TABLE old_sync_state").await?;

        // =====================================================================
        // Phase G: Migrate alarm_thresholds
        // =====================================================================

        // Drop old constraints
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds DROP CONSTRAINT IF EXISTS alarm_thresholds_parameter_id_key",
        ).await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds DROP CONSTRAINT IF EXISTS fk_alarm_thresholds_parameter",
        )
        .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_alarm_thresholds_parameter")
            .await?;

        // Add new columns
        db.execute_unprepared("ALTER TABLE alarm_thresholds ADD COLUMN IF NOT EXISTS site_id UUID")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds ADD COLUMN IF NOT EXISTS string_alarm_values JSONB",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds ADD COLUMN IF NOT EXISTS string_warning_values JSONB",
        )
        .await?;

        // Remap: old parameter_id → site_id from site_parameters, parameter_id → global parameter
        db.execute_unprepared(
            r"UPDATE alarm_thresholds at_row
            SET site_id = sp.site_id, parameter_id = sp.parameter_id
            FROM site_parameters sp WHERE sp.id = at_row.parameter_id",
        )
        .await?;

        // New FKs + partial unique indexes
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds ADD CONSTRAINT fk_alarm_thresholds_parameter FOREIGN KEY (parameter_id) REFERENCES parameters(id) ON DELETE CASCADE",
        ).await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds ADD CONSTRAINT fk_alarm_thresholds_site FOREIGN KEY (site_id) REFERENCES sites(id) ON DELETE CASCADE",
        ).await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_alarm_thresholds_param_site ON alarm_thresholds (parameter_id, site_id) WHERE site_id IS NOT NULL",
        ).await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_alarm_thresholds_param_global ON alarm_thresholds (parameter_id) WHERE site_id IS NULL",
        ).await?;

        // =====================================================================
        // Phase H: Source mappings + nullability fixes
        // =====================================================================

        // Update entity_type
        db.execute_unprepared(
            "UPDATE source_mappings SET entity_type = 'site_parameter' WHERE entity_type = 'parameter'",
        ).await?;

        // Nullability fixes
        db.execute_unprepared("ALTER TABLE sensors ALTER COLUMN is_lab_instrument DROP NOT NULL")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE public_exposed_parameters ALTER COLUMN conversion_factor DROP NOT NULL",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public_exposed_parameters ALTER COLUMN conversion_offset DROP NOT NULL",
        )
        .await?;

        // =====================================================================
        // Phase I: Cleanup
        // =====================================================================

        // Decompress device_status before dropping
        db.execute_unprepared(
            "SELECT remove_compression_policy('device_status', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            r"DO $$
            DECLARE
                chunk REGCLASS;
            BEGIN
                FOR chunk IN
                    SELECT format('%I.%I', chunk_schema, chunk_name)::regclass
                    FROM timescaledb_information.chunks
                    WHERE hypertable_name = 'device_status'
                      AND is_compressed = true
                LOOP
                    PERFORM decompress_chunk(chunk, if_compressed => true);
                END LOOP;
            END $$",
        )
        .await?;

        db.execute_unprepared("DROP TABLE IF EXISTS device_status CASCADE")
            .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS calibrations CASCADE")
            .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS old_parameters CASCADE")
            .await?;

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "Cannot auto-rollback. Restore from pg_dump backup.".into(),
        ))
    }
}
