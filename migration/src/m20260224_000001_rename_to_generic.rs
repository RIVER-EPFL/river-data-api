use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // ================================================================
        // 1. DROP CONTINUOUS AGGREGATES AND THEIR POLICIES
        // ================================================================
        // Must drop policies before dropping the views
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

        // ================================================================
        // 2. DROP COMPRESSION POLICIES
        // ================================================================
        // Compression policies reference the old column names via segmentby
        db.execute_unprepared(
            "SELECT remove_compression_policy('readings', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_compression_policy('device_status', if_exists => true)",
        )
        .await?;

        // ================================================================
        // 3. DROP INDEXES THAT REFERENCE OLD COLUMN/TABLE NAMES
        // ================================================================
        db.execute_unprepared("DROP INDEX IF EXISTS zones_name_lower_idx")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS stations_name_lower_idx")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_sensors_station_name")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_sensors_vaisala_location_id")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_readings_sensor_time")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_alarm_thresholds_sensor")
            .await?;

        // ================================================================
        // 4. DROP FOREIGN KEY CONSTRAINTS (they reference old table/column names)
        // ================================================================
        db.execute_unprepared("ALTER TABLE stations DROP CONSTRAINT IF EXISTS fk_stations_zone")
            .await?;
        db.execute_unprepared("ALTER TABLE sensors DROP CONSTRAINT IF EXISTS fk_sensors_station")
            .await?;
        db.execute_unprepared("ALTER TABLE readings DROP CONSTRAINT IF EXISTS fk_readings_sensor")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE device_status DROP CONSTRAINT IF EXISTS fk_device_status_sensor",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE calibrations DROP CONSTRAINT IF EXISTS fk_calibrations_sensor",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE sync_state DROP CONSTRAINT IF EXISTS fk_sync_state_sensor",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds DROP CONSTRAINT IF EXISTS fk_alarm_thresholds_sensor",
        )
        .await?;

        // ================================================================
        // 5. RENAME COLUMNS (before renaming tables, so table names still match)
        // ================================================================

        // stations: zone_id -> project_id
        db.execute_unprepared("ALTER TABLE stations RENAME COLUMN zone_id TO project_id")
            .await?;

        // stations: vaisala_node_id -> source_node_id
        db.execute_unprepared(
            "ALTER TABLE stations RENAME COLUMN vaisala_node_id TO source_node_id",
        )
        .await?;

        // stations: vaisala_path -> source_path
        db.execute_unprepared("ALTER TABLE stations RENAME COLUMN vaisala_path TO source_path")
            .await?;

        // sensors: station_id -> site_id
        db.execute_unprepared("ALTER TABLE sensors RENAME COLUMN station_id TO site_id")
            .await?;

        // sensors: vaisala_location_id -> source_location_id
        db.execute_unprepared(
            "ALTER TABLE sensors RENAME COLUMN vaisala_location_id TO source_location_id",
        )
        .await?;

        // readings: sensor_id -> parameter_id
        db.execute_unprepared("ALTER TABLE readings RENAME COLUMN sensor_id TO parameter_id")
            .await?;

        // device_status: sensor_id -> parameter_id
        db.execute_unprepared("ALTER TABLE device_status RENAME COLUMN sensor_id TO parameter_id")
            .await?;

        // calibrations: sensor_id -> parameter_id
        db.execute_unprepared("ALTER TABLE calibrations RENAME COLUMN sensor_id TO parameter_id")
            .await?;

        // sync_state: sensor_id -> parameter_id
        db.execute_unprepared("ALTER TABLE sync_state RENAME COLUMN sensor_id TO parameter_id")
            .await?;

        // alarm_thresholds: sensor_id -> parameter_id
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds RENAME COLUMN sensor_id TO parameter_id",
        )
        .await?;

        // zones: vaisala_path -> source_path
        db.execute_unprepared("ALTER TABLE zones RENAME COLUMN vaisala_path TO source_path")
            .await?;

        // ================================================================
        // 6. RENAME TABLES
        // ================================================================
        db.execute_unprepared("ALTER TABLE zones RENAME TO projects")
            .await?;
        db.execute_unprepared("ALTER TABLE stations RENAME TO sites")
            .await?;
        db.execute_unprepared("ALTER TABLE sensors RENAME TO parameters")
            .await?;

        // ================================================================
        // 7. RECREATE FOREIGN KEY CONSTRAINTS WITH NEW NAMES
        // ================================================================
        db.execute_unprepared(
            "ALTER TABLE sites ADD CONSTRAINT fk_sites_project
             FOREIGN KEY (project_id) REFERENCES projects(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE parameters ADD CONSTRAINT fk_parameters_site
             FOREIGN KEY (site_id) REFERENCES sites(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE readings ADD CONSTRAINT fk_readings_parameter
             FOREIGN KEY (parameter_id) REFERENCES parameters(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE device_status ADD CONSTRAINT fk_device_status_parameter
             FOREIGN KEY (parameter_id) REFERENCES parameters(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE calibrations ADD CONSTRAINT fk_calibrations_parameter
             FOREIGN KEY (parameter_id) REFERENCES parameters(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE sync_state ADD CONSTRAINT fk_sync_state_parameter
             FOREIGN KEY (parameter_id) REFERENCES parameters(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds ADD CONSTRAINT fk_alarm_thresholds_parameter
             FOREIGN KEY (parameter_id) REFERENCES parameters(id)
             ON DELETE CASCADE",
        )
        .await?;

        // ================================================================
        // 8. RECREATE INDEXES WITH NEW NAMES
        // ================================================================
        db.execute_unprepared(
            "CREATE UNIQUE INDEX projects_name_lower_idx ON projects (LOWER(name))",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX sites_name_lower_idx ON sites (LOWER(name))",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX idx_parameters_site_name ON parameters (site_id, name)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX idx_parameters_source_location_id ON parameters (source_location_id)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX idx_readings_parameter_time ON readings (parameter_id, time DESC)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX idx_alarm_thresholds_parameter ON alarm_thresholds (parameter_id)",
        )
        .await?;

        // ================================================================
        // 9. UPDATE PARAMETER NAMES (strip station prefix)
        // ================================================================

        // Martigny: M* prefix
        db.execute_unprepared("UPDATE parameters SET name = 'WaterDepthmm' WHERE name = 'MDepthmm'")
            .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'CDOMppb' WHERE name = 'MCDOMppb'")
            .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'TurbiNTU' WHERE name = 'MTurbNTU'")
            .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'BattV' WHERE name = 'MBattV'")
            .await?;
        db.execute_unprepared(
            "UPDATE parameters SET name = 'WaterTempdegC' WHERE name = 'MDOdegC'",
        )
        .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'ConduScm' WHERE name = 'MConduSCm'")
            .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'DOuM' WHERE name = 'MDOuM'")
            .await?;
        db.execute_unprepared(
            "UPDATE parameters SET name = 'CondTempdegC' WHERE name = 'MCondTdegC'",
        )
        .await?;

        // Saxon: S* prefix
        db.execute_unprepared("UPDATE parameters SET name = 'WaterDepthmm' WHERE name = 'SDepthmm'")
            .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'CDOMppb' WHERE name = 'SCDOMppb'")
            .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'TurbiNTU' WHERE name = 'STurbNTU'")
            .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'BattV' WHERE name = 'SBattV'")
            .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'DOuM' WHERE name = 'SDOuM'")
            .await?;
        db.execute_unprepared(
            "UPDATE parameters SET name = 'WaterTempdegC' WHERE name = 'SDOdegC'",
        )
        .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'ConduScm' WHERE name = 'SConduScm'")
            .await?;
        db.execute_unprepared(
            "UPDATE parameters SET name = 'CondTempdegC' WHERE name = 'SCondTdegC'",
        )
        .await?;

        // Les Dailles: D* prefix
        db.execute_unprepared("UPDATE parameters SET name = 'DOuM' WHERE name = 'DDOuM'")
            .await?;
        db.execute_unprepared(
            "UPDATE parameters SET name = 'WaterTempdegC' WHERE name = 'DDOTdegC'",
        )
        .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'BattV' WHERE name = 'DBattV'")
            .await?;

        // Verbier: V* prefix
        db.execute_unprepared("UPDATE parameters SET name = 'DOuM' WHERE name = 'VDOuM'")
            .await?;
        db.execute_unprepared(
            "UPDATE parameters SET name = 'WaterTempdegC' WHERE name = 'VDOTdegC'",
        )
        .await?;
        db.execute_unprepared("UPDATE parameters SET name = 'BattV' WHERE name = 'VBattV'")
            .await?;

        // ================================================================
        // 10. RE-ADD COMPRESSION POLICIES WITH NEW COLUMN NAMES
        // ================================================================
        db.execute_unprepared(
            r"ALTER TABLE readings SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'parameter_id'
            )",
        )
        .await?;
        db.execute_unprepared("SELECT add_compression_policy('readings', INTERVAL '30 days')")
            .await?;

        db.execute_unprepared(
            r"ALTER TABLE device_status SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'parameter_id'
            )",
        )
        .await?;
        db.execute_unprepared(
            "SELECT add_compression_policy('device_status', INTERVAL '90 days')",
        )
        .await?;

        // ================================================================
        // 11. RECREATE CONTINUOUS AGGREGATES WITH parameter_id
        // ================================================================
        db.execute_unprepared(
            r"
            CREATE MATERIALIZED VIEW readings_hourly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 hour', time) AS bucket,
                parameter_id,
                AVG(value) AS avg_value,
                MIN(value) AS min_value,
                MAX(value) AS max_value,
                COUNT(*) AS count,
                STDDEV(value) AS stddev_value
            FROM readings
            GROUP BY time_bucket('1 hour', time), parameter_id
            WITH NO DATA
            ",
        )
        .await?;

        db.execute_unprepared(
            r"
            CREATE MATERIALIZED VIEW readings_daily
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 day', time) AS bucket,
                parameter_id,
                AVG(value) AS avg_value,
                MIN(value) AS min_value,
                MAX(value) AS max_value,
                COUNT(*) AS count,
                STDDEV(value) AS stddev_value
            FROM readings
            GROUP BY time_bucket('1 day', time), parameter_id
            WITH NO DATA
            ",
        )
        .await?;

        db.execute_unprepared(
            r"
            CREATE MATERIALIZED VIEW readings_weekly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 week', time) AS bucket,
                parameter_id,
                AVG(value) AS avg_value,
                MIN(value) AS min_value,
                MAX(value) AS max_value,
                COUNT(*) AS count,
                STDDEV(value) AS stddev_value
            FROM readings
            GROUP BY time_bucket('1 week', time), parameter_id
            WITH NO DATA
            ",
        )
        .await?;

        db.execute_unprepared(
            r"
            CREATE MATERIALIZED VIEW readings_monthly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 month', time) AS bucket,
                parameter_id,
                AVG(value) AS avg_value,
                MIN(value) AS min_value,
                MAX(value) AS max_value,
                COUNT(*) AS count,
                STDDEV(value) AS stddev_value
            FROM readings
            GROUP BY time_bucket('1 month', time), parameter_id
            WITH NO DATA
            ",
        )
        .await?;

        // ================================================================
        // 12. RE-ADD CONTINUOUS AGGREGATE REFRESH POLICIES
        // ================================================================
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

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // ================================================================
        // 1. DROP CONTINUOUS AGGREGATES AND THEIR POLICIES
        // ================================================================
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

        // ================================================================
        // 2. DROP COMPRESSION POLICIES
        // ================================================================
        db.execute_unprepared(
            "SELECT remove_compression_policy('readings', if_exists => true)",
        )
        .await?;
        db.execute_unprepared(
            "SELECT remove_compression_policy('device_status', if_exists => true)",
        )
        .await?;

        // ================================================================
        // 3. DROP INDEXES WITH NEW NAMES
        // ================================================================
        db.execute_unprepared("DROP INDEX IF EXISTS projects_name_lower_idx")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS sites_name_lower_idx")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_parameters_site_name")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_parameters_source_location_id")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_readings_parameter_time")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_alarm_thresholds_parameter")
            .await?;

        // ================================================================
        // 4. DROP FOREIGN KEY CONSTRAINTS WITH NEW NAMES
        // ================================================================
        db.execute_unprepared("ALTER TABLE sites DROP CONSTRAINT IF EXISTS fk_sites_project")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE parameters DROP CONSTRAINT IF EXISTS fk_parameters_site",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE readings DROP CONSTRAINT IF EXISTS fk_readings_parameter",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE device_status DROP CONSTRAINT IF EXISTS fk_device_status_parameter",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE calibrations DROP CONSTRAINT IF EXISTS fk_calibrations_parameter",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE sync_state DROP CONSTRAINT IF EXISTS fk_sync_state_parameter",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds DROP CONSTRAINT IF EXISTS fk_alarm_thresholds_parameter",
        )
        .await?;

        // ================================================================
        // 5. RENAME TABLES BACK
        // ================================================================
        db.execute_unprepared("ALTER TABLE projects RENAME TO zones")
            .await?;
        db.execute_unprepared("ALTER TABLE sites RENAME TO stations")
            .await?;
        db.execute_unprepared("ALTER TABLE parameters RENAME TO sensors")
            .await?;

        // ================================================================
        // 6. RENAME COLUMNS BACK
        // ================================================================

        // zones: source_path -> vaisala_path
        db.execute_unprepared("ALTER TABLE zones RENAME COLUMN source_path TO vaisala_path")
            .await?;

        // stations: project_id -> zone_id
        db.execute_unprepared("ALTER TABLE stations RENAME COLUMN project_id TO zone_id")
            .await?;

        // stations: source_node_id -> vaisala_node_id
        db.execute_unprepared(
            "ALTER TABLE stations RENAME COLUMN source_node_id TO vaisala_node_id",
        )
        .await?;

        // stations: source_path -> vaisala_path
        db.execute_unprepared("ALTER TABLE stations RENAME COLUMN source_path TO vaisala_path")
            .await?;

        // sensors: site_id -> station_id
        db.execute_unprepared("ALTER TABLE sensors RENAME COLUMN site_id TO station_id")
            .await?;

        // sensors: source_location_id -> vaisala_location_id
        db.execute_unprepared(
            "ALTER TABLE sensors RENAME COLUMN source_location_id TO vaisala_location_id",
        )
        .await?;

        // readings: parameter_id -> sensor_id
        db.execute_unprepared("ALTER TABLE readings RENAME COLUMN parameter_id TO sensor_id")
            .await?;

        // device_status: parameter_id -> sensor_id
        db.execute_unprepared("ALTER TABLE device_status RENAME COLUMN parameter_id TO sensor_id")
            .await?;

        // calibrations: parameter_id -> sensor_id
        db.execute_unprepared("ALTER TABLE calibrations RENAME COLUMN parameter_id TO sensor_id")
            .await?;

        // sync_state: parameter_id -> sensor_id
        db.execute_unprepared("ALTER TABLE sync_state RENAME COLUMN parameter_id TO sensor_id")
            .await?;

        // alarm_thresholds: parameter_id -> sensor_id
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds RENAME COLUMN parameter_id TO sensor_id",
        )
        .await?;

        // ================================================================
        // 7. REVERSE PARAMETER NAME UPDATES (generic -> Vaisala convention)
        // ================================================================
        // We need to use source_location_id to disambiguate parameters with the
        // same generic name across different stations.

        // Martigny (location IDs: 1270, 1272, 1282, 1288, 1301, 1346, 1424, 1483)
        db.execute_unprepared(
            "UPDATE sensors SET name = 'MDepthmm' WHERE name = 'WaterDepthmm' AND vaisala_location_id = 1270",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'MCDOMppb' WHERE name = 'CDOMppb' AND vaisala_location_id = 1272",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'MTurbNTU' WHERE name = 'TurbiNTU' AND vaisala_location_id = 1282",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'MBattV' WHERE name = 'BattV' AND vaisala_location_id = 1288",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'MDOdegC' WHERE name = 'WaterTempdegC' AND vaisala_location_id = 1301",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'MConduSCm' WHERE name = 'ConduScm' AND vaisala_location_id = 1346",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'MDOuM' WHERE name = 'DOuM' AND vaisala_location_id = 1424",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'MCondTdegC' WHERE name = 'CondTempdegC' AND vaisala_location_id = 1483",
        )
        .await?;

        // Saxon (location IDs: 1248, 1260, 1280, 1290, 1310, 1312, 1341, 1481)
        db.execute_unprepared(
            "UPDATE sensors SET name = 'SDepthmm' WHERE name = 'WaterDepthmm' AND vaisala_location_id = 1248",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'SCDOMppb' WHERE name = 'CDOMppb' AND vaisala_location_id = 1260",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'STurbNTU' WHERE name = 'TurbiNTU' AND vaisala_location_id = 1280",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'SBattV' WHERE name = 'BattV' AND vaisala_location_id = 1290",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'SDOuM' WHERE name = 'DOuM' AND vaisala_location_id = 1310",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'SDOdegC' WHERE name = 'WaterTempdegC' AND vaisala_location_id = 1312",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'SConduScm' WHERE name = 'ConduScm' AND vaisala_location_id = 1341",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'SCondTdegC' WHERE name = 'CondTempdegC' AND vaisala_location_id = 1481",
        )
        .await?;

        // Les Dailles (location IDs: 1462, 1464, 1466)
        db.execute_unprepared(
            "UPDATE sensors SET name = 'DDOuM' WHERE name = 'DOuM' AND vaisala_location_id = 1462",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'DDOTdegC' WHERE name = 'WaterTempdegC' AND vaisala_location_id = 1464",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'DBattV' WHERE name = 'BattV' AND vaisala_location_id = 1466",
        )
        .await?;

        // Verbier (location IDs: 1436, 1439, 1445)
        db.execute_unprepared(
            "UPDATE sensors SET name = 'VDOuM' WHERE name = 'DOuM' AND vaisala_location_id = 1436",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'VDOTdegC' WHERE name = 'WaterTempdegC' AND vaisala_location_id = 1439",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sensors SET name = 'VBattV' WHERE name = 'BattV' AND vaisala_location_id = 1445",
        )
        .await?;

        // ================================================================
        // 8. RECREATE FOREIGN KEY CONSTRAINTS WITH ORIGINAL NAMES
        // ================================================================
        db.execute_unprepared(
            "ALTER TABLE stations ADD CONSTRAINT fk_stations_zone
             FOREIGN KEY (zone_id) REFERENCES zones(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE sensors ADD CONSTRAINT fk_sensors_station
             FOREIGN KEY (station_id) REFERENCES stations(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE readings ADD CONSTRAINT fk_readings_sensor
             FOREIGN KEY (sensor_id) REFERENCES sensors(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE device_status ADD CONSTRAINT fk_device_status_sensor
             FOREIGN KEY (sensor_id) REFERENCES sensors(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE calibrations ADD CONSTRAINT fk_calibrations_sensor
             FOREIGN KEY (sensor_id) REFERENCES sensors(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE sync_state ADD CONSTRAINT fk_sync_state_sensor
             FOREIGN KEY (sensor_id) REFERENCES sensors(id)",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_thresholds ADD CONSTRAINT fk_alarm_thresholds_sensor
             FOREIGN KEY (sensor_id) REFERENCES sensors(id)
             ON DELETE CASCADE",
        )
        .await?;

        // ================================================================
        // 9. RECREATE INDEXES WITH ORIGINAL NAMES
        // ================================================================
        db.execute_unprepared(
            "CREATE UNIQUE INDEX zones_name_lower_idx ON zones (LOWER(name))",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX stations_name_lower_idx ON stations (LOWER(name))",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX idx_sensors_station_name ON sensors (station_id, name)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX idx_sensors_vaisala_location_id ON sensors (vaisala_location_id)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX idx_readings_sensor_time ON readings (sensor_id, time DESC)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX idx_alarm_thresholds_sensor ON alarm_thresholds (sensor_id)",
        )
        .await?;

        // ================================================================
        // 10. RE-ADD COMPRESSION POLICIES WITH ORIGINAL COLUMN NAMES
        // ================================================================
        db.execute_unprepared(
            r"ALTER TABLE readings SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'sensor_id'
            )",
        )
        .await?;
        db.execute_unprepared("SELECT add_compression_policy('readings', INTERVAL '30 days')")
            .await?;

        db.execute_unprepared(
            r"ALTER TABLE device_status SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'sensor_id'
            )",
        )
        .await?;
        db.execute_unprepared(
            "SELECT add_compression_policy('device_status', INTERVAL '90 days')",
        )
        .await?;

        // ================================================================
        // 11. RECREATE CONTINUOUS AGGREGATES WITH sensor_id
        // ================================================================
        db.execute_unprepared(
            r"
            CREATE MATERIALIZED VIEW readings_hourly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 hour', time) AS bucket,
                sensor_id,
                AVG(value) AS avg_value,
                MIN(value) AS min_value,
                MAX(value) AS max_value,
                COUNT(*) AS count,
                STDDEV(value) AS stddev_value
            FROM readings
            GROUP BY time_bucket('1 hour', time), sensor_id
            WITH NO DATA
            ",
        )
        .await?;

        db.execute_unprepared(
            r"
            CREATE MATERIALIZED VIEW readings_daily
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 day', time) AS bucket,
                sensor_id,
                AVG(value) AS avg_value,
                MIN(value) AS min_value,
                MAX(value) AS max_value,
                COUNT(*) AS count,
                STDDEV(value) AS stddev_value
            FROM readings
            GROUP BY time_bucket('1 day', time), sensor_id
            WITH NO DATA
            ",
        )
        .await?;

        db.execute_unprepared(
            r"
            CREATE MATERIALIZED VIEW readings_weekly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 week', time) AS bucket,
                sensor_id,
                AVG(value) AS avg_value,
                MIN(value) AS min_value,
                MAX(value) AS max_value,
                COUNT(*) AS count,
                STDDEV(value) AS stddev_value
            FROM readings
            GROUP BY time_bucket('1 week', time), sensor_id
            WITH NO DATA
            ",
        )
        .await?;

        db.execute_unprepared(
            r"
            CREATE MATERIALIZED VIEW readings_monthly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 month', time) AS bucket,
                sensor_id,
                AVG(value) AS avg_value,
                MIN(value) AS min_value,
                MAX(value) AS max_value,
                COUNT(*) AS count,
                STDDEV(value) AS stddev_value
            FROM readings
            GROUP BY time_bucket('1 month', time), sensor_id
            WITH NO DATA
            ",
        )
        .await?;

        // ================================================================
        // 12. RE-ADD CONTINUOUS AGGREGATE REFRESH POLICIES
        // ================================================================
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

        Ok(())
    }
}
