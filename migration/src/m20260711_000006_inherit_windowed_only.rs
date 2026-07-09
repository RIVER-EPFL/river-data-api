use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    // Scope the parameter-inherit trigger to windowed calibrations. Instant (lab grab) curves carry
    // no parameter by design: the parameter is decided on the grab reading, not the curve. The
    // original trigger inherited a parameter for any NULL-parameter row, which stamped a spurious
    // parameter onto instant curves once they became creatable via the CRUD endpoint.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                CREATE OR REPLACE FUNCTION inherit_calibration_parameter_id() RETURNS trigger AS $fn$
                BEGIN
                    IF NEW.parameter_id IS NULL AND NEW.mode = 'windowed' THEN
                        SELECT parameter_id INTO NEW.parameter_id
                        FROM sensor_calibrations
                        WHERE sensor_id = NEW.sensor_id AND parameter_id IS NOT NULL
                        ORDER BY valid_from
                        LIMIT 1;
                    END IF;
                    RETURN NEW;
                END;
                $fn$ LANGUAGE plpgsql;
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
                "#,
            )
            .await?;
        Ok(())
    }
}
