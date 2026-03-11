use sea_orm_migration::prelude::*;

/// v0.2.0 baseline schema — the schema deployed on the production database.
///
/// On an existing v0.2.0 database this migration is already recorded in
/// `seaql_migrations` and will be skipped.  On a fresh database it creates
/// the v0.2.0 schema so that the subsequent upgrade migration can transform
/// it to the current (HEAD) schema.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // ========== PROJECTS ==========
        manager
            .create_table(
                Table::create()
                    .table(Projects::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Projects::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(ColumnDef::new(Projects::Name).string_len(64).not_null())
                    .col(ColumnDef::new(Projects::Description).text())
                    .col(
                        ColumnDef::new(Projects::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(ColumnDef::new(Projects::DiscoveredAt).timestamp_with_time_zone())
                    .to_owned(),
            )
            .await?;

        manager
            .get_connection()
            .execute_unprepared(
                "CREATE UNIQUE INDEX IF NOT EXISTS projects_name_lower_idx ON projects (LOWER(name))",
            )
            .await?;

        // ========== SITES ==========
        manager
            .create_table(
                Table::create()
                    .table(Sites::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Sites::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(ColumnDef::new(Sites::ProjectId).uuid())
                    .col(ColumnDef::new(Sites::Name).string_len(64).not_null())
                    .col(ColumnDef::new(Sites::Latitude).double())
                    .col(ColumnDef::new(Sites::Longitude).double())
                    .col(ColumnDef::new(Sites::AltitudeM).double())
                    .col(
                        ColumnDef::new(Sites::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(ColumnDef::new(Sites::DiscoveredAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_sites_project")
                            .from(Sites::Table, Sites::ProjectId)
                            .to(Projects::Table, Projects::Id),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .get_connection()
            .execute_unprepared(
                "CREATE UNIQUE INDEX IF NOT EXISTS sites_name_lower_idx ON sites (LOWER(name))",
            )
            .await?;

        // ========== PARAMETERS (site-specific, v0.2.0 schema) ==========
        manager
            .create_table(
                Table::create()
                    .table(Parameters::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Parameters::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(ColumnDef::new(Parameters::SiteId).uuid().not_null())
                    .col(ColumnDef::new(Parameters::Name).string_len(64).not_null())
                    .col(
                        ColumnDef::new(Parameters::SensorType)
                            .string_len(64)
                            .not_null(),
                    )
                    .col(ColumnDef::new(Parameters::DisplayUnits).string_len(32))
                    .col(ColumnDef::new(Parameters::UnitsName).string_len(64))
                    .col(ColumnDef::new(Parameters::UnitsMin).double())
                    .col(ColumnDef::new(Parameters::UnitsMax).double())
                    .col(ColumnDef::new(Parameters::DecimalPlaces).small_integer())
                    .col(ColumnDef::new(Parameters::ChannelId).integer())
                    .col(
                        ColumnDef::new(Parameters::SampleIntervalSec)
                            .integer()
                            .default(600),
                    )
                    .col(ColumnDef::new(Parameters::IsActive).boolean().default(true))
                    .col(
                        ColumnDef::new(Parameters::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(
                        ColumnDef::new(Parameters::UpdatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(ColumnDef::new(Parameters::DiscoveredAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(Parameters::DeviceSerialNumber).string_len(64))
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_parameters_site")
                            .from(Parameters::Table, Parameters::SiteId)
                            .to(Sites::Table, Sites::Id),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_parameters_site_name")
                    .table(Parameters::Table)
                    .col(Parameters::SiteId)
                    .col(Parameters::Name)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // ========== READINGS (TimescaleDB Hypertable) ==========
        manager
            .create_table(
                Table::create()
                    .table(Readings::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(Readings::ParameterId).uuid().not_null())
                    .col(
                        ColumnDef::new(Readings::Time)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Readings::Value).double().not_null())
                    .col(ColumnDef::new(Readings::Logged).boolean().default(true))
                    .primary_key(
                        Index::create()
                            .col(Readings::ParameterId)
                            .col(Readings::Time),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_readings_parameter")
                            .from(Readings::Table, Readings::ParameterId)
                            .to(Parameters::Table, Parameters::Id),
                    )
                    .to_owned(),
            )
            .await?;

        let db = manager.get_connection();

        db.execute_unprepared(
            "SELECT create_hypertable('readings', 'time', chunk_time_interval => INTERVAL '7 days', if_not_exists => TRUE)",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_readings_param_time ON readings (parameter_id, time DESC)",
        )
        .await?;

        // ========== DEVICE STATUS (TimescaleDB Hypertable) ==========
        manager
            .create_table(
                Table::create()
                    .table(DeviceStatus::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(DeviceStatus::ParameterId).uuid().not_null())
                    .col(
                        ColumnDef::new(DeviceStatus::Time)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(DeviceStatus::BatteryLevel).double())
                    .col(ColumnDef::new(DeviceStatus::BatteryState).integer())
                    .col(ColumnDef::new(DeviceStatus::SignalQuality).integer())
                    .col(ColumnDef::new(DeviceStatus::Status).text())
                    .col(ColumnDef::new(DeviceStatus::Unreachable).boolean())
                    .primary_key(
                        Index::create()
                            .col(DeviceStatus::ParameterId)
                            .col(DeviceStatus::Time),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_device_status_parameter")
                            .from(DeviceStatus::Table, DeviceStatus::ParameterId)
                            .to(Parameters::Table, Parameters::Id),
                    )
                    .to_owned(),
            )
            .await?;

        db.execute_unprepared(
            "SELECT create_hypertable('device_status', 'time', chunk_time_interval => INTERVAL '30 days', if_not_exists => TRUE)",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_device_status_param_time ON device_status (parameter_id, time DESC)",
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
                    .col(ColumnDef::new(Calibrations::ParameterId).uuid().not_null())
                    .col(ColumnDef::new(Calibrations::Slope).double().not_null())
                    .col(ColumnDef::new(Calibrations::Intercept).double().not_null())
                    .col(
                        ColumnDef::new(Calibrations::AppliedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Calibrations::Notes).text())
                    .col(
                        ColumnDef::new(Calibrations::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_calibrations_parameter")
                            .from(Calibrations::Table, Calibrations::ParameterId)
                            .to(Parameters::Table, Parameters::Id),
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
                        ColumnDef::new(SyncState::ParameterId)
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
                            .name("fk_sync_state_parameter")
                            .from(SyncState::Table, SyncState::ParameterId)
                            .to(Parameters::Table, Parameters::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // ========== ALARM THRESHOLDS ==========
        manager
            .create_table(
                Table::create()
                    .table(AlarmThresholds::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AlarmThresholds::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(
                        ColumnDef::new(AlarmThresholds::ParameterId)
                            .uuid()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(AlarmThresholds::WarningMin).double())
                    .col(ColumnDef::new(AlarmThresholds::WarningMax).double())
                    .col(ColumnDef::new(AlarmThresholds::AlarmMin).double())
                    .col(ColumnDef::new(AlarmThresholds::AlarmMax).double())
                    .col(ColumnDef::new(AlarmThresholds::Description).text())
                    .col(
                        ColumnDef::new(AlarmThresholds::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(
                        ColumnDef::new(AlarmThresholds::UpdatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_alarm_thresholds_parameter")
                            .from(AlarmThresholds::Table, AlarmThresholds::ParameterId)
                            .to(Parameters::Table, Parameters::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_alarm_thresholds_parameter ON alarm_thresholds (parameter_id)",
        )
        .await?;

        // ========== SOURCE MAPPINGS ==========
        manager
            .create_table(
                Table::create()
                    .table(SourceMappings::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(SourceMappings::EntityType)
                            .string_len(32)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SourceMappings::SourceKey)
                            .integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(SourceMappings::EntityId).uuid().not_null())
                    .col(ColumnDef::new(SourceMappings::SourceName).string_len(256))
                    .primary_key(
                        Index::create()
                            .col(SourceMappings::EntityType)
                            .col(SourceMappings::SourceKey),
                    )
                    .to_owned(),
            )
            .await?;

        // ========== CONTINUOUS AGGREGATES ==========
        db.execute_unprepared(
            r"
            CREATE MATERIALIZED VIEW IF NOT EXISTS readings_hourly
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
            CREATE MATERIALIZED VIEW IF NOT EXISTS readings_daily
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
            CREATE MATERIALIZED VIEW IF NOT EXISTS readings_weekly
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
            CREATE MATERIALIZED VIEW IF NOT EXISTS readings_monthly
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

        // ========== COMPRESSION POLICIES ==========
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

        db.execute_unprepared("SELECT add_compression_policy('device_status', INTERVAL '90 days')")
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
            "SELECT remove_compression_policy('device_status', if_exists => true)",
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
            "source_mappings",
            "alarm_thresholds",
            "sync_state",
            "calibrations",
            "device_status",
            "readings",
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

// ========== Iden Enums ==========

#[derive(DeriveIden)]
enum Projects {
    Table,
    Id,
    Name,
    Description,
    CreatedAt,
    DiscoveredAt,
}

#[derive(DeriveIden)]
enum Sites {
    Table,
    Id,
    ProjectId,
    Name,
    Latitude,
    Longitude,
    AltitudeM,
    CreatedAt,
    DiscoveredAt,
}

#[derive(DeriveIden)]
enum Parameters {
    Table,
    Id,
    SiteId,
    Name,
    SensorType,
    DisplayUnits,
    UnitsName,
    UnitsMin,
    UnitsMax,
    DecimalPlaces,
    ChannelId,
    SampleIntervalSec,
    IsActive,
    CreatedAt,
    UpdatedAt,
    DiscoveredAt,
    DeviceSerialNumber,
}

#[derive(DeriveIden)]
enum Readings {
    Table,
    ParameterId,
    Time,
    Value,
    Logged,
}

#[derive(DeriveIden)]
enum DeviceStatus {
    Table,
    ParameterId,
    Time,
    BatteryLevel,
    BatteryState,
    SignalQuality,
    #[sea_orm(iden = "device_status")]
    Status,
    Unreachable,
}

#[derive(DeriveIden)]
enum Calibrations {
    Table,
    Id,
    ParameterId,
    Slope,
    Intercept,
    AppliedAt,
    Notes,
    CreatedAt,
}

#[derive(DeriveIden)]
enum SyncState {
    Table,
    ParameterId,
    LastDataTime,
    LastSyncAttempt,
    SyncStatus,
    ErrorMessage,
    RetryCount,
    LastFullSync,
}

#[derive(DeriveIden)]
enum AlarmThresholds {
    Table,
    Id,
    ParameterId,
    WarningMin,
    WarningMax,
    AlarmMin,
    AlarmMax,
    Description,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum SourceMappings {
    Table,
    EntityType,
    SourceKey,
    EntityId,
    SourceName,
}
