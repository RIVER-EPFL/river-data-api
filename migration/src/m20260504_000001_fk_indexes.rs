use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            CREATE INDEX IF NOT EXISTS idx_sites_project_id
                ON sites (project_id);

            CREATE INDEX IF NOT EXISTS idx_sensor_deployments_sensor_id
                ON sensor_deployments (sensor_id);
            CREATE INDEX IF NOT EXISTS idx_sensor_deployments_site_id
                ON sensor_deployments (site_id);

            CREATE INDEX IF NOT EXISTS idx_notes_site_id
                ON notes (site_id);

            CREATE INDEX IF NOT EXISTS idx_sensors_parameter_id
                ON sensors (parameter_id);

            CREATE INDEX IF NOT EXISTS idx_standard_curves_parameter_id
                ON standard_curves (parameter_id);

            CREATE INDEX IF NOT EXISTS idx_pub_exp_params_parameter_id
                ON public_exposed_parameters (parameter_id);
            CREATE INDEX IF NOT EXISTS idx_pub_exp_params_project_id
                ON public_exposed_parameters (project_id);

            CREATE INDEX IF NOT EXISTS idx_alarm_thresholds_site_id
                ON alarm_thresholds (site_id)
                WHERE site_id IS NOT NULL;

            CREATE INDEX IF NOT EXISTS idx_data_streams_pairing_plan_id
                ON data_streams (pairing_plan_id)
                WHERE pairing_plan_id IS NOT NULL;

            CREATE INDEX IF NOT EXISTS idx_sync_svc_creds_service_id
                ON sync_service_credentials (service_id)
                WHERE service_id IS NOT NULL;

            CREATE INDEX IF NOT EXISTS idx_sync_svc_tokens_service_id
                ON sync_service_tokens (service_id);

            CREATE INDEX IF NOT EXISTS idx_site_parameters_parameter_id
                ON site_parameters (parameter_id);
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            DROP INDEX IF EXISTS idx_sites_project_id;
            DROP INDEX IF EXISTS idx_sensor_deployments_sensor_id;
            DROP INDEX IF EXISTS idx_sensor_deployments_site_id;
            DROP INDEX IF EXISTS idx_notes_site_id;
            DROP INDEX IF EXISTS idx_sensors_parameter_id;
            DROP INDEX IF EXISTS idx_standard_curves_parameter_id;
            DROP INDEX IF EXISTS idx_pub_exp_params_parameter_id;
            DROP INDEX IF EXISTS idx_pub_exp_params_project_id;
            DROP INDEX IF EXISTS idx_alarm_thresholds_site_id;
            DROP INDEX IF EXISTS idx_data_streams_pairing_plan_id;
            DROP INDEX IF EXISTS idx_sync_svc_creds_service_id;
            DROP INDEX IF EXISTS idx_sync_svc_tokens_service_id;
            DROP INDEX IF EXISTS idx_site_parameters_parameter_id;
            "#,
        )
        .await?;

        Ok(())
    }
}
