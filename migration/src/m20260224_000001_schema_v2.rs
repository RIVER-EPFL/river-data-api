use sea_orm_migration::prelude::*;

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
                        ColumnDef::new(Projects::DataSource)
                            .string_len(64)
                            .not_null()
                            .default("vaisala"),
                    )
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
            .execute_unprepared("CREATE UNIQUE INDEX IF NOT EXISTS projects_name_lower_idx ON projects (LOWER(name))")
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

        // ========== PARAMETER TYPES (global catalog) ==========
        manager
            .create_table(
                Table::create()
                    .table(ParameterTypes::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ParameterTypes::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(
                        ColumnDef::new(ParameterTypes::Name)
                            .string_len(64)
                            .unique_key()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ParameterTypes::DisplayName)
                            .string_len(128)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ParameterTypes::DefaultUnits)
                            .string_len(32)
                            .not_null(),
                    )
                    .col(ColumnDef::new(ParameterTypes::Description).text())
                    .col(
                        ColumnDef::new(ParameterTypes::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .to_owned(),
            )
            .await?;

        // ========== SENSORS (physical instruments) ==========
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
                    .col(
                        ColumnDef::new(Sensors::SerialNumber)
                            .string_len(64)
                            .unique_key()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Sensors::Name).string_len(128))
                    .col(
                        ColumnDef::new(Sensors::ParameterTypeId)
                            .uuid()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Sensors::Manufacturer).string_len(128))
                    .col(ColumnDef::new(Sensors::Model).string_len(128))
                    .col(ColumnDef::new(Sensors::IsActive).boolean().default(true))
                    .col(ColumnDef::new(Sensors::Notes).text())
                    .col(
                        ColumnDef::new(Sensors::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_sensors_parameter_type")
                            .from(Sensors::Table, Sensors::ParameterTypeId)
                            .to(ParameterTypes::Table, ParameterTypes::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // ========== SENSOR CALIBRATIONS ==========
        manager
            .create_table(
                Table::create()
                    .table(SensorCalibrations::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(SensorCalibrations::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(
                        ColumnDef::new(SensorCalibrations::SensorId)
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SensorCalibrations::Slope)
                            .double()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SensorCalibrations::Intercept)
                            .double()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SensorCalibrations::ValidFrom)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(SensorCalibrations::PerformedBy).string_len(128))
                    .col(ColumnDef::new(SensorCalibrations::Notes).text())
                    .col(
                        ColumnDef::new(SensorCalibrations::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_sensor_calibrations_sensor")
                            .from(SensorCalibrations::Table, SensorCalibrations::SensorId)
                            .to(Sensors::Table, Sensors::Id),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_sensor_calibrations_sensor_valid_from ON sensor_calibrations (sensor_id, valid_from DESC)",
            )
            .await?;

        // ========== DERIVED PARAMETER DEFINITIONS ==========
        manager
            .create_table(
                Table::create()
                    .table(DerivedParameterDefinitions::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(DerivedParameterDefinitions::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(
                        ColumnDef::new(DerivedParameterDefinitions::Name)
                            .string_len(128)
                            .unique_key()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DerivedParameterDefinitions::DisplayName)
                            .string_len(256)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DerivedParameterDefinitions::Units)
                            .string_len(32)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DerivedParameterDefinitions::Formula)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(DerivedParameterDefinitions::Description).text())
                    .col(
                        ColumnDef::new(DerivedParameterDefinitions::RequiredParameterTypes)
                            .json_binary()
                            .not_null()
                            .default("[]"),
                    )
                    .col(
                        ColumnDef::new(DerivedParameterDefinitions::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .to_owned(),
            )
            .await?;

        // ========== PARAMETERS ==========
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
                    .col(ColumnDef::new(Parameters::ParameterTypeId).uuid().not_null())
                    .col(ColumnDef::new(Parameters::Name).string_len(64).not_null())
                    .col(ColumnDef::new(Parameters::SensorType).string_len(64).not_null())
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
                        ColumnDef::new(Parameters::IsDerived)
                            .boolean()
                            .default(false),
                    )
                    .col(ColumnDef::new(Parameters::DerivedDefinitionId).uuid())
                    .col(ColumnDef::new(Parameters::VariableMappings).json_binary())
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
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_parameters_site")
                            .from(Parameters::Table, Parameters::SiteId)
                            .to(Sites::Table, Sites::Id),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_parameters_parameter_type")
                            .from(Parameters::Table, Parameters::ParameterTypeId)
                            .to(ParameterTypes::Table, ParameterTypes::Id),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_parameters_derived_definition")
                            .from(Parameters::Table, Parameters::DerivedDefinitionId)
                            .to(
                                DerivedParameterDefinitions::Table,
                                DerivedParameterDefinitions::Id,
                            ),
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

        // ========== SENSOR DEPLOYMENTS ==========
        manager
            .create_table(
                Table::create()
                    .table(SensorDeployments::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(SensorDeployments::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(
                        ColumnDef::new(SensorDeployments::SensorId)
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SensorDeployments::ParameterId)
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SensorDeployments::DeployedFrom)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SensorDeployments::DeployedUntil)
                            .timestamp_with_time_zone(),
                    )
                    .col(
                        ColumnDef::new(SensorDeployments::DeploymentType)
                            .string_len(32)
                            .not_null()
                            .default("permanent"),
                    )
                    .col(ColumnDef::new(SensorDeployments::Notes).text())
                    .col(
                        ColumnDef::new(SensorDeployments::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_sensor_deployments_sensor")
                            .from(SensorDeployments::Table, SensorDeployments::SensorId)
                            .to(Sensors::Table, Sensors::Id),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_sensor_deployments_parameter")
                            .from(SensorDeployments::Table, SensorDeployments::ParameterId)
                            .to(Parameters::Table, Parameters::Id),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_sensor_deployments_sensor_from ON sensor_deployments (sensor_id, deployed_from DESC)",
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
                    .col(
                        ColumnDef::new(SourceMappings::EntityId)
                            .uuid()
                            .not_null(),
                    )
                    .col(ColumnDef::new(SourceMappings::SourceName).string_len(256))
                    .primary_key(
                        Index::create()
                            .col(SourceMappings::EntityType)
                            .col(SourceMappings::SourceKey),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_source_mappings_entity_id")
                    .table(SourceMappings::Table)
                    .col(SourceMappings::EntityId)
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
                    .col(ColumnDef::new(Readings::ParameterId).uuid().not_null())
                    .col(ColumnDef::new(Readings::RawValue).double().not_null())
                    .col(ColumnDef::new(Readings::CalibratedValue).double())
                    .col(ColumnDef::new(Readings::SensorId).uuid())
                    .col(ColumnDef::new(Readings::CalibrationId).uuid())
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
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_readings_sensor")
                            .from(Readings::Table, Readings::SensorId)
                            .to(Sensors::Table, Sensors::Id),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_readings_calibration")
                            .from(Readings::Table, Readings::CalibrationId)
                            .to(SensorCalibrations::Table, SensorCalibrations::Id),
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
            "CREATE INDEX IF NOT EXISTS idx_readings_parameter_time ON readings (parameter_id, time DESC)",
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
                    .col(ColumnDef::new(DeviceStatus::ParameterId).uuid().not_null())
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

        // ========== API TOKENS ==========
        manager
            .create_table(
                Table::create()
                    .table(ApiTokens::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ApiTokens::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(
                        ColumnDef::new(ApiTokens::Name)
                            .string_len(128)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ApiTokens::TokenHash)
                            .string_len(64)
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(ApiTokens::ProjectScope).uuid())
                    .col(
                        ColumnDef::new(ApiTokens::Permissions)
                            .json_binary()
                            .not_null()
                            .default("{}"),
                    )
                    .col(
                        ColumnDef::new(ApiTokens::IsActive)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .col(
                        ColumnDef::new(ApiTokens::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(ColumnDef::new(ApiTokens::ExpiresAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(ApiTokens::LastUsedAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(ApiTokens::CreatedBy).string_len(128))
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_api_tokens_project_scope")
                            .from(ApiTokens::Table, ApiTokens::ProjectScope)
                            .to(Projects::Table, Projects::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // ========== DATA IMPORTS ==========
        manager
            .create_table(
                Table::create()
                    .table(DataImports::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(DataImports::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .extra("DEFAULT gen_random_uuid()"),
                    )
                    .col(ColumnDef::new(DataImports::ProjectId).uuid())
                    .col(
                        ColumnDef::new(DataImports::SourceType)
                            .string_len(64)
                            .not_null(),
                    )
                    .col(ColumnDef::new(DataImports::FileName).string_len(256))
                    .col(
                        ColumnDef::new(DataImports::Status)
                            .string_len(32)
                            .not_null()
                            .default("pending"),
                    )
                    .col(
                        ColumnDef::new(DataImports::RowsImported)
                            .integer()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(DataImports::RowsFailed)
                            .integer()
                            .default(0),
                    )
                    .col(ColumnDef::new(DataImports::ErrorMessage).text())
                    .col(ColumnDef::new(DataImports::StartedAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(DataImports::CompletedAt).timestamp_with_time_zone())
                    .col(
                        ColumnDef::new(DataImports::CreatedAt)
                            .timestamp_with_time_zone()
                            .extra("DEFAULT NOW()"),
                    )
                    .col(ColumnDef::new(DataImports::CreatedBy).string_len(128))
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_data_imports_project")
                            .from(DataImports::Table, DataImports::ProjectId)
                            .to(Projects::Table, Projects::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // ========== CONTINUOUS AGGREGATES (TimescaleDB-specific) ==========
        db.execute_unprepared(
            r"
            CREATE MATERIALIZED VIEW IF NOT EXISTS readings_hourly
            WITH (timescaledb.continuous) AS
            SELECT
                time_bucket('1 hour', time) AS bucket,
                parameter_id,
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
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
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
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
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
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
                AVG(COALESCE(calibrated_value, raw_value)) AS avg_value,
                MIN(COALESCE(calibrated_value, raw_value)) AS min_value,
                MAX(COALESCE(calibrated_value, raw_value)) AS max_value,
                COUNT(*) AS count,
                STDDEV(COALESCE(calibrated_value, raw_value)) AS stddev_value
            FROM readings
            GROUP BY time_bucket('1 month', time), parameter_id
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

        // Drop tables in reverse dependency order
        manager
            .drop_table(
                Table::drop()
                    .table(DataImports::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(ApiTokens::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(AlarmThresholds::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table(SyncState::Table).if_exists().to_owned())
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
            .drop_table(
                Table::drop()
                    .table(SourceMappings::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(SensorDeployments::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table(Parameters::Table).if_exists().to_owned())
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(DerivedParameterDefinitions::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(SensorCalibrations::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table(Sensors::Table).if_exists().to_owned())
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(ParameterTypes::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table(Sites::Table).if_exists().to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Projects::Table).if_exists().to_owned())
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
pub enum Projects {
    Table,
    Id,
    Name,
    Description,
    DataSource,
    CreatedAt,
    DiscoveredAt,
}

#[derive(DeriveIden)]
pub enum Sites {
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
pub enum ParameterTypes {
    Table,
    Id,
    Name,
    DisplayName,
    DefaultUnits,
    Description,
    CreatedAt,
}

#[derive(DeriveIden)]
pub enum Sensors {
    Table,
    Id,
    SerialNumber,
    Name,
    ParameterTypeId,
    Manufacturer,
    Model,
    IsActive,
    Notes,
    CreatedAt,
}

#[derive(DeriveIden)]
pub enum SensorCalibrations {
    Table,
    Id,
    SensorId,
    Slope,
    Intercept,
    ValidFrom,
    PerformedBy,
    Notes,
    CreatedAt,
}

#[derive(DeriveIden)]
pub enum DerivedParameterDefinitions {
    Table,
    Id,
    Name,
    DisplayName,
    Units,
    Formula,
    Description,
    RequiredParameterTypes,
    CreatedAt,
}

#[derive(DeriveIden)]
pub enum Parameters {
    Table,
    Id,
    SiteId,
    ParameterTypeId,
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
    IsDerived,
    DerivedDefinitionId,
    VariableMappings,
    CreatedAt,
    UpdatedAt,
    DiscoveredAt,
}

#[derive(DeriveIden)]
pub enum SensorDeployments {
    Table,
    Id,
    SensorId,
    ParameterId,
    DeployedFrom,
    DeployedUntil,
    DeploymentType,
    Notes,
    CreatedAt,
}

#[derive(DeriveIden)]
pub enum SourceMappings {
    Table,
    EntityType,
    SourceKey,
    EntityId,
    SourceName,
}

#[derive(DeriveIden)]
pub enum Readings {
    Table,
    Time,
    ParameterId,
    RawValue,
    CalibratedValue,
    SensorId,
    CalibrationId,
    Logged,
}

#[derive(DeriveIden)]
#[allow(clippy::enum_variant_names)]
pub enum DeviceStatus {
    Table,
    Time,
    ParameterId,
    BatteryLevel,
    BatteryState,
    SignalQuality,
    #[sea_orm(iden = "device_status")]
    StatusValue,
    Unreachable,
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
pub enum ApiTokens {
    Table,
    Id,
    Name,
    TokenHash,
    ProjectScope,
    Permissions,
    IsActive,
    CreatedAt,
    ExpiresAt,
    LastUsedAt,
    CreatedBy,
}

#[derive(DeriveIden)]
pub enum DataImports {
    Table,
    Id,
    ProjectId,
    SourceType,
    FileName,
    Status,
    RowsImported,
    RowsFailed,
    ErrorMessage,
    StartedAt,
    CompletedAt,
    CreatedAt,
    CreatedBy,
}
