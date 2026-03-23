use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260323_000002_sensor_wiring"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // 1. Add metadata JSONB column to sensors
        db.execute_unprepared(
            r#"ALTER TABLE sensors ADD COLUMN IF NOT EXISTS metadata JSONB"#,
        )
        .await?;

        // 2. Add sensor_id FK to data_streams
        db.execute_unprepared(
            r#"ALTER TABLE data_streams ADD COLUMN IF NOT EXISTS sensor_id UUID REFERENCES sensors(id) ON DELETE SET NULL"#,
        )
        .await?;

        // 3. Index on data_streams.sensor_id
        db.execute_unprepared(
            r#"CREATE INDEX IF NOT EXISTS idx_data_streams_sensor_id ON data_streams (sensor_id) WHERE sensor_id IS NOT NULL"#,
        )
        .await?;

        // 4. Partial unique index on sensors for dedup when serial_number is set
        db.execute_unprepared(
            r#"CREATE UNIQUE INDEX IF NOT EXISTS idx_sensors_serial_parameter ON sensors (serial_number, parameter_id) WHERE serial_number IS NOT NULL"#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"DROP INDEX IF EXISTS idx_sensors_serial_parameter"#,
        )
        .await?;

        db.execute_unprepared(
            r#"DROP INDEX IF EXISTS idx_data_streams_sensor_id"#,
        )
        .await?;

        db.execute_unprepared(
            r#"ALTER TABLE data_streams DROP COLUMN IF EXISTS sensor_id"#,
        )
        .await?;

        db.execute_unprepared(
            r#"ALTER TABLE sensors DROP COLUMN IF EXISTS metadata"#,
        )
        .await?;

        Ok(())
    }
}
