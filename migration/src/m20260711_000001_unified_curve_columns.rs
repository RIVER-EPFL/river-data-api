use sea_orm_migration::prelude::*;

/// Extend `sensor_calibrations` into the unified curve model: a curve is now either `windowed`
/// (field sensor, auto-applied over `[valid_from, valid_until)`) or `instant` (lab grab, applied to
/// a single reading and chosen by `name`). `parameter_id` is set for windowed field channels (a
/// multi-parameter instrument gets one curve per channel) and NULL for lab instant curves (the
/// parameter is decided at the grab). This migration is purely additive: existing rows become
/// `windowed` and backfill their `parameter_id` from the sensor they belong to (this runs before
/// `sensors.parameter_id` is dropped in a later migration). Folding the now-empty `standard_curves`
/// table and moving `sensor_deployments.parameter_id` to authored are handled alongside the
/// operations rewrite, since they change behavior their callers depend on.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            ALTER TABLE sensor_calibrations
                ADD COLUMN name TEXT,
                ADD COLUMN mode TEXT NOT NULL DEFAULT 'windowed',
                ADD COLUMN parameter_id UUID,
                ADD COLUMN r_squared DOUBLE PRECISION
            "#,
        )
        .await?;

        db.execute_unprepared(
            "ALTER TABLE sensor_calibrations
                ADD CONSTRAINT sensor_calibrations_mode_check
                CHECK (mode IN ('windowed', 'instant'))",
        )
        .await?;

        db.execute_unprepared(
            "ALTER TABLE sensor_calibrations
                ADD CONSTRAINT sensor_calibrations_parameter_id_fkey
                FOREIGN KEY (parameter_id) REFERENCES parameters(id) ON DELETE SET NULL",
        )
        .await?;

        // Backfill the per-channel parameter from the owning sensor while sensors.parameter_id still
        // exists. Windowed field calibrations keep this; lab instant curves (added later) leave it NULL.
        db.execute_unprepared(
            "UPDATE sensor_calibrations sc
                SET parameter_id = s.parameter_id
                FROM sensors s
                WHERE s.id = sc.sensor_id",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sensor_calibrations_parameter_id
                ON sensor_calibrations (parameter_id)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_sensor_calibrations_parameter_id")
            .await?;
        db.execute_unprepared(
            r#"
            ALTER TABLE sensor_calibrations
                DROP CONSTRAINT IF EXISTS sensor_calibrations_parameter_id_fkey,
                DROP CONSTRAINT IF EXISTS sensor_calibrations_mode_check,
                DROP COLUMN IF EXISTS name,
                DROP COLUMN IF EXISTS mode,
                DROP COLUMN IF EXISTS parameter_id,
                DROP COLUMN IF EXISTS r_squared
            "#,
        )
        .await?;
        Ok(())
    }
}
