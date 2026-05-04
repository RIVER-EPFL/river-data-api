use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Add output_parameter_id column (nullable initially for existing rows)
        db.execute_unprepared(
            r#"ALTER TABLE derived_parameter_definitions
               ADD COLUMN IF NOT EXISTS output_parameter_id UUID REFERENCES parameters(id)"#,
        )
        .await?;

        // Back-fill: for each existing derived definition, create the output
        // parameter if it doesn't exist and link it.
        db.execute_unprepared(
            r#"
            INSERT INTO parameters (id, name, display_name, default_units, category, data_type, description)
            SELECT gen_random_uuid(), dpd.name, dpd.display_name, dpd.units, 'derived', 'float', dpd.description
            FROM derived_parameter_definitions dpd
            WHERE NOT EXISTS (
                SELECT 1 FROM parameters p WHERE p.name = dpd.name
            )
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            UPDATE derived_parameter_definitions dpd
            SET output_parameter_id = p.id
            FROM parameters p
            WHERE p.name = dpd.name
              AND dpd.output_parameter_id IS NULL
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            r#"ALTER TABLE derived_parameter_definitions DROP COLUMN IF EXISTS output_parameter_id"#,
        )
        .await?;
        Ok(())
    }
}
