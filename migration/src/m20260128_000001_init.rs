use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // ========== ZONES ==========
        manager
            .create_table(
                Table::create()
                    .table(Zones::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Zones::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(ColumnDef::new(Zones::Name).string_len(64).not_null())
                    .col(ColumnDef::new(Zones::VaisalaPath).string_len(256))
                    .col(ColumnDef::new(Zones::Description).text())
                    .col(
                        ColumnDef::new(Zones::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(ColumnDef::new(Zones::DiscoveredAt).timestamp_with_time_zone())
                    .to_owned(),
            )
            .await?;

        // Case-insensitive unique index on zone name
        manager
            .get_connection()
            .execute_unprepared("CREATE UNIQUE INDEX zones_name_lower_idx ON zones (LOWER(name))")
            .await?;

        // ========== STATIONS ==========
        manager
            .create_table(
                Table::create()
                    .table(Stations::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Stations::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(ColumnDef::new(Stations::ZoneId).uuid())
                    .col(ColumnDef::new(Stations::Name).string_len(64).not_null())
                    .col(
                        ColumnDef::new(Stations::VaisalaNodeId)
                            .integer()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(Stations::VaisalaPath).string_len(256))
                    .col(ColumnDef::new(Stations::Latitude).double())
                    .col(ColumnDef::new(Stations::Longitude).double())
                    .col(ColumnDef::new(Stations::AltitudeM).double())
                    .col(
                        ColumnDef::new(Stations::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(ColumnDef::new(Stations::DiscoveredAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_stations_zone")
                            .from(Stations::Table, Stations::ZoneId)
                            .to(Zones::Table, Zones::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // Case-insensitive unique index on station name
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE UNIQUE INDEX stations_name_lower_idx ON stations (LOWER(name))",
            )
            .await?;

        // ========== SENSORS ==========
        manager
            .create_table(
                Table::create()
                    .table(Sensors::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Sensors::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(ColumnDef::new(Sensors::StationId).uuid().not_null())
                    .col(
                        ColumnDef::new(Sensors::VaisalaLocationId)
                            .integer()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(Sensors::Name).string_len(64).not_null())
                    .col(ColumnDef::new(Sensors::SensorType).string_len(64).not_null())
                    .col(ColumnDef::new(Sensors::DisplayUnits).string_len(32))
                    .col(ColumnDef::new(Sensors::UnitsName).string_len(64))
                    .col(ColumnDef::new(Sensors::UnitsMin).double())
                    .col(ColumnDef::new(Sensors::UnitsMax).double())
                    .col(ColumnDef::new(Sensors::DecimalPlaces).small_integer())
                    .col(ColumnDef::new(Sensors::DeviceSerialNumber).string_len(32))
                    .col(ColumnDef::new(Sensors::ProbeSerialNumber).string_len(32))
                    .col(ColumnDef::new(Sensors::ChannelId).integer())
                    .col(
                        ColumnDef::new(Sensors::SampleIntervalSec)
                            .integer()
                            .default(600),
                    )
                    .col(ColumnDef::new(Sensors::IsActive).boolean().default(true))
                    .col(
                        ColumnDef::new(Sensors::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(
                        ColumnDef::new(Sensors::UpdatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(ColumnDef::new(Sensors::DiscoveredAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_sensors_station")
                            .from(Sensors::Table, Sensors::StationId)
                            .to(Stations::Table, Stations::Id),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_sensors_station_name")
                    .table(Sensors::Table)
                    .col(Sensors::StationId)
                    .col(Sensors::Name)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_sensors_vaisala_location_id")
                    .table(Sensors::Table)
                    .col(Sensors::VaisalaLocationId)
                    .to_owned(),
            )
            .await?;

        // ========== READINGS (TimescaleDB Hypertable) ==========
        manager
            .create_table(
                Table::create()
                    .table(Readings::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Readings::Time)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Readings::SensorId).uuid().not_null())
                    .col(ColumnDef::new(Readings::Value).double().not_null())
                    .col(ColumnDef::new(Readings::Logged).boolean().default(true))
                    .primary_key(
                        Index::create()
                            .col(Readings::SensorId)
                            .col(Readings::Time),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_readings_sensor")
                            .from(Readings::Table, Readings::SensorId)
                            .to(Sensors::Table, Sensors::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // Convert to TimescaleDB hypertable (requires raw SQL)
        let db = manager.get_connection();
        db.execute_unprepared(
            "SELECT create_hypertable('readings', 'time', chunk_time_interval => INTERVAL '7 days')",
        )
        .await?;

        // Index for efficient sensor_id lookups with time range
        db.execute_unprepared(
            "CREATE INDEX idx_readings_sensor_time ON readings (sensor_id, time DESC)",
        )
        .await?;

        // ========== DEVICE STATUS (TimescaleDB Hypertable) ==========
        manager
            .create_table(
                Table::create()
                    .table(DeviceStatus::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(DeviceStatus::Time)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(DeviceStatus::SensorId).uuid().not_null())
                    .col(ColumnDef::new(DeviceStatus::BatteryLevel).small_integer())
                    .col(ColumnDef::new(DeviceStatus::BatteryState).small_integer())
                    .col(ColumnDef::new(DeviceStatus::SignalQuality).small_integer())
                    .col(ColumnDef::new(DeviceStatus::StatusValue).string_len(32))
                    .col(
                        ColumnDef::new(DeviceStatus::Unreachable)
                            .boolean()
                            .default(false),
                    )
                    .primary_key(
                        Index::create()
                            .col(DeviceStatus::SensorId)
                            .col(DeviceStatus::Time),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_device_status_sensor")
                            .from(DeviceStatus::Table, DeviceStatus::SensorId)
                            .to(Sensors::Table, Sensors::Id),
                    )
                    .to_owned(),
            )
            .await?;

        db.execute_unprepared(
            "SELECT create_hypertable('device_status', 'time', chunk_time_interval => INTERVAL '30 days')",
        )
        .await?;

        // ========== CALIBRATIONS ==========
        manager
            .create_table(
                Table::create()
                    .table(Calibrations::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Calibrations::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(ColumnDef::new(Calibrations::SensorId).uuid().not_null())
                    .col(
                        ColumnDef::new(Calibrations::CalibrationTime)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Calibrations::PerformedBy).string_len(128))
                    .col(ColumnDef::new(Calibrations::Notes).text())
                    .col(
                        ColumnDef::new(Calibrations::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_calibrations_sensor")
                            .from(Calibrations::Table, Calibrations::SensorId)
                            .to(Sensors::Table, Sensors::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // ========== SYNC STATE ==========
        manager
            .create_table(
                Table::create()
                    .table(SyncState::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(SyncState::SensorId)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(SyncState::LastDataTime).timestamp_with_time_zone())
                    .col(ColumnDef::new(SyncState::LastSyncAttempt).timestamp_with_time_zone())
                    .col(
                        ColumnDef::new(SyncState::SyncStatus)
                            .string_len(32)
                            .default("pending"),
                    )
                    .col(ColumnDef::new(SyncState::ErrorMessage).text())
                    .col(ColumnDef::new(SyncState::RetryCount).integer().default(0))
                    .col(ColumnDef::new(SyncState::LastFullSync).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_sync_state_sensor")
                            .from(SyncState::Table, SyncState::SensorId)
                            .to(Sensors::Table, Sensors::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // ========== CONTINUOUS AGGREGATES (TimescaleDB-specific) ==========
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

        // Continuous aggregate refresh policies
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

        // ========== COMPRESSION POLICIES (TimescaleDB-specific) ==========
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

        db.execute_unprepared("SELECT add_compression_policy('device_status', INTERVAL '90 days')")
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Remove compression policies
        db.execute_unprepared(
            "SELECT remove_compression_policy('device_status', if_exists => true)",
        )
        .await
        .ok();
        db.execute_unprepared("SELECT remove_compression_policy('readings', if_exists => true)")
            .await
            .ok();

        // Remove continuous aggregate policies and views
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_monthly', if_exists => true)",
        )
        .await
        .ok();
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_weekly', if_exists => true)",
        )
        .await
        .ok();
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_daily', if_exists => true)",
        )
        .await
        .ok();
        db.execute_unprepared(
            "SELECT remove_continuous_aggregate_policy('readings_hourly', if_exists => true)",
        )
        .await
        .ok();

        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_monthly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_weekly CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_daily CASCADE")
            .await?;
        db.execute_unprepared("DROP MATERIALIZED VIEW IF EXISTS readings_hourly CASCADE")
            .await?;

        // Drop tables in reverse order of dependencies
        manager
            .drop_table(Table::drop().table(SyncState::Table).if_exists().to_owned())
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(Calibrations::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(DeviceStatus::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table(Readings::Table).if_exists().to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Sensors::Table).if_exists().to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Stations::Table).if_exists().to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Zones::Table).if_exists().to_owned())
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
pub enum Zones {
    Table,
    Id,
    Name,
    VaisalaPath,
    Description,
    CreatedAt,
    DiscoveredAt,
}

#[derive(DeriveIden)]
pub enum Stations {
    Table,
    Id,
    ZoneId,
    Name,
    VaisalaNodeId,
    VaisalaPath,
    Latitude,
    Longitude,
    AltitudeM,
    CreatedAt,
    DiscoveredAt,
}

#[derive(DeriveIden)]
pub enum Sensors {
    Table,
    Id,
    StationId,
    VaisalaLocationId,
    Name,
    SensorType,
    DisplayUnits,
    UnitsName,
    UnitsMin,
    UnitsMax,
    DecimalPlaces,
    DeviceSerialNumber,
    ProbeSerialNumber,
    ChannelId,
    SampleIntervalSec,
    IsActive,
    CreatedAt,
    UpdatedAt,
    DiscoveredAt,
}

#[derive(DeriveIden)]
pub enum Readings {
    Table,
    Time,
    SensorId,
    Value,
    Logged,
}

#[derive(DeriveIden)]
#[allow(clippy::enum_variant_names)]
pub enum DeviceStatus {
    Table,
    Time,
    SensorId,
    BatteryLevel,
    BatteryState,
    SignalQuality,
    #[sea_orm(iden = "device_status")]
    StatusValue,
    Unreachable,
}

#[derive(DeriveIden)]
enum Calibrations {
    Table,
    Id,
    SensorId,
    CalibrationTime,
    PerformedBy,
    Notes,
    CreatedAt,
}

#[derive(DeriveIden)]
enum SyncState {
    Table,
    SensorId,
    LastDataTime,
    LastSyncAttempt,
    SyncStatus,
    ErrorMessage,
    RetryCount,
    LastFullSync,
}
