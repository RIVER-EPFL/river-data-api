use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Externalised staging for a CSV import: the handler bulk-loads the parsed rows here keyed by
        // a per-request `import_token`, then enqueues a `csv_import` worker job that reads them back.
        // This lets any replica run the import (the parsed `Vec` no longer lives only in the handler's
        // memory, so a dead replica can't strand the work). Only the variable per-row fields are
        // staged; the readings constants (replicate_index, logged, measurement_type, is_flagged) are
        // re-applied by the job. The job DELETEs its token's rows on completion.
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS csv_import_staging ( \
                 import_token UUID NOT NULL, \
                 stream_id UUID NOT NULL, \
                 site_id UUID, \
                 parameter_id UUID, \
                 time TIMESTAMPTZ NOT NULL, \
                 raw_value DOUBLE PRECISION NOT NULL, \
                 sensor_id UUID, \
                 calibration_id UUID, \
                 deployment_id UUID \
             )",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_csv_import_staging_token \
             ON csv_import_staging (import_token)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_csv_import_staging_token")
            .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS csv_import_staging")
            .await?;
        Ok(())
    }
}
