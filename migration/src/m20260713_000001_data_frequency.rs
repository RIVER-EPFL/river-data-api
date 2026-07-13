use sea_orm_migration::prelude::*;

/// Make data frequency an explicit, editable classification instead of a hardcoded ingest constant.
///
/// - `sensors.data_frequency` ('high' | 'low'): what cadence a device produces. Field sondes are
///   'high' (10-min streams → readings.measurement_type 'continuous'); lab instruments are 'low'
///   (campaign/grab results → 'spot'). Kept separate from `is_lab_instrument`, which says what the
///   device *is*; `data_frequency` says what its data *is*.
/// - `data_streams.measurement_type` ('continuous' | 'spot' | 'derived' | NULL): stream-level
///   default for streams with no sensor attribution (metalp/nomis imports). NULL = defer to the
///   sensor's `data_frequency`, then fall back to 'continuous'.
///
/// Ingest resolves per reading: explicit override → stream default → sensor frequency → 'continuous'.
/// Metalp/nomis streams are classified 'spot' here (lab campaign data); their existing readings are
/// retagged separately via the tracked measurement_retag job, not in a migration, because the
/// readings hypertable has compressed chunks.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            ALTER TABLE sensors
                ADD COLUMN data_frequency VARCHAR(16) NOT NULL DEFAULT 'high';
            ALTER TABLE sensors
                ADD CONSTRAINT sensors_data_frequency_check
                CHECK (data_frequency IN ('high', 'low'));
            UPDATE sensors SET data_frequency = 'low' WHERE is_lab_instrument = true;
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            ALTER TABLE data_streams
                ADD COLUMN measurement_type VARCHAR(32);
            ALTER TABLE data_streams
                ADD CONSTRAINT data_streams_measurement_type_check
                CHECK (measurement_type IN ('continuous', 'spot', 'derived'));
            UPDATE data_streams SET measurement_type = 'spot'
                WHERE source_system IN ('metalp', 'nomis');
            UPDATE data_streams SET measurement_type = 'spot'
                WHERE source_system = 'grab_sample';
            UPDATE data_streams SET measurement_type = 'derived'
                WHERE source_system = 'derived';
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            r#"
            ALTER TABLE data_streams
                DROP CONSTRAINT IF EXISTS data_streams_measurement_type_check,
                DROP COLUMN IF EXISTS measurement_type;
            ALTER TABLE sensors
                DROP CONSTRAINT IF EXISTS sensors_data_frequency_check,
                DROP COLUMN IF EXISTS data_frequency;
            "#,
        )
        .await?;
        Ok(())
    }
}
