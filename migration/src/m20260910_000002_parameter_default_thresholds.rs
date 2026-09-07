use sea_orm_migration::prelude::*;

/// One home for a parameter's own bounds. `parameters.default_{warning,alarm}_{min,max}` were the
/// third tier of the threshold resolution and said the same thing as a global `alarm_thresholds`
/// row, at a different priority and through a different editor. Each parameter carrying any default
/// becomes that global row, and the columns go.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
    INSERT INTO alarm_thresholds (parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max, description)
    SELECT p.id, NULL, p.default_warning_min, p.default_warning_max, p.default_alarm_min, p.default_alarm_max,
           'Parameter default'
      FROM parameters p
     WHERE p.default_warning_min IS NOT NULL
        OR p.default_warning_max IS NOT NULL
        OR p.default_alarm_min IS NOT NULL
        OR p.default_alarm_max IS NOT NULL
    ON CONFLICT DO NOTHING;

    ALTER TABLE parameters
        DROP COLUMN IF EXISTS default_warning_min,
        DROP COLUMN IF EXISTS default_warning_max,
        DROP COLUMN IF EXISTS default_alarm_min,
        DROP COLUMN IF EXISTS default_alarm_max;
";

/// The columns come back empty: the global rows the up migration wrote are the thresholds now, and
/// copying them back would restore the duplication this removed.
const DOWN: &str = r"
    ALTER TABLE parameters
        ADD COLUMN IF NOT EXISTS default_warning_min double precision,
        ADD COLUMN IF NOT EXISTS default_warning_max double precision,
        ADD COLUMN IF NOT EXISTS default_alarm_min double precision,
        ADD COLUMN IF NOT EXISTS default_alarm_max double precision;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }
}
