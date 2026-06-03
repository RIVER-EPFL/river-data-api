use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // alarm_type discriminator and the string-alarm columns were never read;
        // every alarm path evaluates numeric min/max. Collapse to numeric-range only.
        db.execute_unprepared(
            r#"
            DROP INDEX IF EXISTS uq_alarm_thresh_param_site_type;
            DROP INDEX IF EXISTS uq_alarm_thresh_param_type_global;
            ALTER TABLE alarm_thresholds
                DROP COLUMN IF EXISTS alarm_type,
                DROP COLUMN IF EXISTS string_alarm_values,
                DROP COLUMN IF EXISTS string_warning_values;
            CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_thresh_param_site
                ON alarm_thresholds (parameter_id, site_id) WHERE site_id IS NOT NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_thresh_param_global
                ON alarm_thresholds (parameter_id) WHERE site_id IS NULL;
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            DROP INDEX IF EXISTS uq_alarm_thresh_param_site;
            DROP INDEX IF EXISTS uq_alarm_thresh_param_global;
            ALTER TABLE alarm_thresholds
                ADD COLUMN IF NOT EXISTS alarm_type VARCHAR(32) NOT NULL DEFAULT 'range',
                ADD COLUMN IF NOT EXISTS string_alarm_values JSONB,
                ADD COLUMN IF NOT EXISTS string_warning_values JSONB;
            CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_thresh_param_site_type
                ON alarm_thresholds (parameter_id, site_id, alarm_type) WHERE site_id IS NOT NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_thresh_param_type_global
                ON alarm_thresholds (parameter_id, alarm_type) WHERE site_id IS NULL;
            "#,
        )
        .await?;

        Ok(())
    }
}
