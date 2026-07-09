use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    // A calibration created without an explicit parameter inherits the sensor's parameter from its
    // existing calibrations. After decoupling, a sensor has no intrinsic parameter, but most
    // calibration-create paths (CrudCrate, the auto identity calibration) don't yet supply one. This
    // keeps a sensor's windowed calibration timeline on a single parameter partition so `valid_until`
    // chaining and window-based reprocess resolve correctly. An explicit parameter_id (a
    // multi-parameter instrument selecting a channel) always wins.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                CREATE OR REPLACE FUNCTION inherit_calibration_parameter_id() RETURNS trigger AS $fn$
                BEGIN
                    IF NEW.parameter_id IS NULL THEN
                        SELECT parameter_id INTO NEW.parameter_id
                        FROM sensor_calibrations
                        WHERE sensor_id = NEW.sensor_id AND parameter_id IS NOT NULL
                        ORDER BY valid_from
                        LIMIT 1;
                    END IF;
                    RETURN NEW;
                END;
                $fn$ LANGUAGE plpgsql;

                DROP TRIGGER IF EXISTS trg_inherit_calibration_parameter_id ON sensor_calibrations;
                CREATE TRIGGER trg_inherit_calibration_parameter_id
                    BEFORE INSERT ON sensor_calibrations
                    FOR EACH ROW EXECUTE FUNCTION inherit_calibration_parameter_id();
                "#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                DROP TRIGGER IF EXISTS trg_inherit_calibration_parameter_id ON sensor_calibrations;
                DROP FUNCTION IF EXISTS inherit_calibration_parameter_id();
                "#,
            )
            .await?;
        Ok(())
    }
}
