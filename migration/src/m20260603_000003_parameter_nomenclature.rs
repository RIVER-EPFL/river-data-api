use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // parameters: name (short code) -> code, display_name (human label) -> name
        db.execute_unprepared(
            r#"
            DROP INDEX IF EXISTS parameters_name_lower_idx;
            ALTER TABLE parameters RENAME COLUMN name TO code;
            ALTER TABLE parameters RENAME COLUMN display_name TO name;
            ALTER TABLE parameters DROP COLUMN IF EXISTS data_type;
            CREATE UNIQUE INDEX IF NOT EXISTS parameters_code_lower_idx ON parameters (LOWER(code));
            "#,
        )
        .await?;

        // category is now strictly the quantity kind; "derived" is tracked by
        // site_parameters.is_derived / derived_parameter_definitions.output_parameter_id.
        db.execute_unprepared(
            r#"
            UPDATE parameters SET category = 'measurement' WHERE category = 'derived';
            ALTER TABLE parameters
                ADD CONSTRAINT parameters_category_check
                CHECK (category IN ('measurement', 'device_health'));
            "#,
        )
        .await?;

        // derived_parameter_definitions: mirror the same rename for consistency.
        db.execute_unprepared(
            r#"
            ALTER TABLE derived_parameter_definitions RENAME COLUMN name TO code;
            ALTER TABLE derived_parameter_definitions RENAME COLUMN display_name TO name;
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            ALTER TABLE derived_parameter_definitions RENAME COLUMN name TO display_name;
            ALTER TABLE derived_parameter_definitions RENAME COLUMN code TO name;
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            ALTER TABLE parameters DROP CONSTRAINT IF EXISTS parameters_category_check;
            DROP INDEX IF EXISTS parameters_code_lower_idx;
            ALTER TABLE parameters ADD COLUMN IF NOT EXISTS data_type VARCHAR(32) NOT NULL DEFAULT 'numeric';
            ALTER TABLE parameters RENAME COLUMN name TO display_name;
            ALTER TABLE parameters RENAME COLUMN code TO name;
            CREATE UNIQUE INDEX IF NOT EXISTS parameters_name_lower_idx ON parameters (LOWER(name));
            "#,
        )
        .await?;

        Ok(())
    }
}
