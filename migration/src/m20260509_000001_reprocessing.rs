use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Add valid_until to sensor_calibrations
        db.execute_unprepared(
            "ALTER TABLE sensor_calibrations ADD COLUMN IF NOT EXISTS valid_until TIMESTAMPTZ",
        )
        .await?;

        // Backfill: set valid_until to the next calibration's valid_from per sensor
        db.execute_unprepared(
            r#"
            WITH ordered AS (
                SELECT id, sensor_id,
                       LEAD(valid_from) OVER (PARTITION BY sensor_id ORDER BY valid_from) AS next_from
                FROM sensor_calibrations
            )
            UPDATE sensor_calibrations sc
            SET valid_until = ordered.next_from
            FROM ordered
            WHERE sc.id = ordered.id AND ordered.next_from IS NOT NULL
            "#,
        )
        .await?;

        // Reprocessing jobs table
        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS reprocessing_jobs (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                sensor_id UUID NOT NULL REFERENCES sensors(id),
                trigger_type TEXT NOT NULL,
                trigger_id UUID,
                status TEXT NOT NULL DEFAULT 'pending',
                readings_updated INT,
                error_message TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                completed_at TIMESTAMPTZ
            )
            "#,
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_reprocessing_jobs_sensor_status \
             ON reprocessing_jobs(sensor_id, status)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS reprocessing_jobs")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE sensor_calibrations DROP COLUMN IF EXISTS valid_until",
        )
        .await?;
        Ok(())
    }
}
