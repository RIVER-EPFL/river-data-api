use sea_orm_migration::prelude::*;

/// A standard curve is a lab curve chosen by hand for a measurement (which microplate the sample
/// went into), not by time. It belongs to one instrument and has no window, which is why it gets its
/// own table instead of a `mode` flag on `sensor_calibrations`: a table with no time columns cannot
/// be picked up by a window query that forgets a filter.
///
/// The rows move across in `m20260813_000004_readings_standard_curve_fk`, once readings have a column
/// to point at.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            r"CREATE TABLE IF NOT EXISTS standard_curves (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                sensor_id UUID NOT NULL REFERENCES sensors(id),
                name TEXT,
                slope DOUBLE PRECISION NOT NULL,
                intercept DOUBLE PRECISION NOT NULL,
                r_squared DOUBLE PRECISION,
                notes TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                created_by TEXT
            );",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_standard_curves_sensor ON standard_curves (sensor_id);",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS standard_curves;")
            .await?;
        Ok(())
    }
}
